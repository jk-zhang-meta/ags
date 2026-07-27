//! Kiro provider — reads/writes both stores Kiro keeps under `~/.kiro`.
//!
//! Kiro ships as two products that share one home directory, and they do not
//! share a session layout. `detect()` fires on `~/.kiro` existing, which either
//! product creates, so reading only one of them renders as "installed, 0
//! sessions" for every user of the other — indistinguishable from having none.
//!
//! ```text
//! <cliRoot>/sessions/cli/<uuid>.{json,jsonl,history}          ← Kiro CLI
//! <ideRoot>/sessions/<workspaceHash>/sess_<uuid>/…            ← Kiro IDE
//! ```
//!
//! # The two roots are not the same variable
//!
//! They are usually the same directory, and they are not the same rule:
//!
//! * `<cliRoot>` is `$KIRO_HOME` when that is set and non-empty, else
//!   `~/.kiro`. This is `kiro-cli`'s own resolver — see [`Kiro::cli_home_dir`],
//!   which quotes it — and `KIRO_HOME` *replaces* the root rather than being a
//!   parent to append `.kiro` to.
//! * `<ideRoot>` is always `~/.kiro`. The Kiro IDE has no relocation variable
//!   at all: `KIRO_HOME` occurs zero times in the entire shipped package. See
//!   [`Kiro::ide_home_dir`].
//!
//! So `KIRO_HOME=/tmp/x` moves the CLI's sessions and leaves the IDE's where
//! they were, and casr has to follow each store to where its own product
//! actually writes. Applying `KIRO_HOME` to both would send the IDE scan to a
//! directory Kiro IDE never writes to — the same defect class as not reading
//! the store at all.
//!
//! One real CLI variable is deliberately not read: `KIRO_TEST_SESSIONS_DIR`,
//! which replaces `sessions/cli` outright. It is part of the CLI's `KIRO_TEST_*`
//! harness family, not a user-facing relocation knob.
//!
//! # Kiro CLI (`kiro-cli`)
//!
//! AWS/Amazon's agentic coding CLI, backed by Amazon Bedrock. Each session is
//! up to three sibling files keyed by the session UUID:
//!
//! ```text
//! <cliRoot>/sessions/cli/<id>.json      ← session metadata + nested session_state
//! <cliRoot>/sessions/cli/<id>.jsonl     ← append-only conversation journal
//! <cliRoot>/sessions/cli/<id>.history   ← plain-text slash-command history (optional)
//! ```
//!
//! # Kiro IDE (the desktop app)
//!
//! A different store, written by the bundled `kiro.kiro-agent` extension. A
//! session is a *directory* under a per-workspace bucket:
//!
//! ```text
//! ~/.kiro/sessions/<bucket>/sess_<uuid>/session.json      ← metadata
//! ~/.kiro/sessions/<bucket>/sess_<uuid>/messages.jsonl    ← one JSON event per line
//! ~/.kiro/sessions/<bucket>/sess_<uuid>/tool-outputs/…    ← spilled tool output
//! ```
//!
//! `<bucket>` is `_global` when the session has no workspace, else the first 16
//! hex characters of `sha256` over the session's absolute workspace paths,
//! normalised, sorted and joined by NUL. It is a one-way hash, so the bucket
//! name is *not* a workspace: the workspace comes from `session.json`'s
//! `workspacePaths`, and is `None` when that array is empty.
//!
//! `session.json` carries `{id, title, agentMode, workspacePaths, createdAt,
//! lastModifiedAt, modelId?, parentSessionId?, …}`. Each `messages.jsonl` line
//! is `{"id", "timestamp", "payload"}` where `payload.type` selects one of
//! twenty-three shapes; the four that carry conversation are `user`,
//! `assistant`, `tool_call` and `tool_result`. The rest are session lifecycle
//! events (`turn_start`, `tombstone`, `usage_summary`, …) and are not messages.
//!
//! ## Why the two scans cannot collide
//!
//! With `KIRO_HOME` unset the two roots coincide, so the scans share a parent.
//! The IDE scan looks for `<sessions>/<dir>/<dir>/session.json`. `sessions/cli`
//! *is* a bucket-shaped directory, but its children are files, not directories,
//! so it yields nothing — and a bucket name is 16 hex characters or `_global`,
//! never `cli`. The id spaces are disjoint too: CLI ids are bare UUIDs, IDE ids
//! are `sess_`-prefixed. `list_sessions` still de-duplicates on the session id,
//! which is the only key both stores agree on.
//!
//! ## `.json` (metadata)
//!
//! ```json
//! {
//!   "session_id": "<uuid>",
//!   "cwd": "/path/to/project",
//!   "created_at": "2026-06-07T14:14:27.290365Z",
//!   "updated_at": "2026-06-07T14:14:36.404077Z",
//!   "title": "…",
//!   "parent_session_id": "<uuid|null>",
//!   "session_created_reason": "subagent|user|…",
//!   "session_state": { "version": "v1", "rts_model_state": { … }, "permissions": { … }, … }
//! }
//! ```
//!
//! ## `.jsonl` (conversation journal)
//!
//! Each line is a versioned envelope `{"version":"v1","kind":<Kind>,"data":{…}}`
//! where `kind` is one of `Prompt` (user), `AssistantMessage` (assistant), or
//! `ToolResults` (tool). The `data.content` array carries typed parts whose
//! own `kind` is `text` | `thinking` | `toolUse` | `toolResult`:
//!
//! - `text`     → `data` is a plain string.
//! - `thinking` → `data` is `{ modelId, text, signature, redactedContent }`.
//! - `toolUse`  → `data` is `{ toolUseId, name, input }`.
//! - `toolResult` → `data` is `{ toolUseId, content: [...], status }`.
//!
//! A `ToolResults` line additionally carries `data.results`, a map keyed by
//! tool-use id with the rich tool invocation/outcome. We preserve it verbatim
//! in the message `extra` so it survives a round-trip.
//!
//! ## Resume
//!
//! ```bash
//! kiro-cli --resume-id <session-id>
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, parse_timestamp,
    reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession, read_dir_reporting,
    store_evidence,
};

/// Provider slug used in canonical metadata.
const SLUG: &str = "kiro";

/// Kiro CLI provider implementation.
pub struct Kiro;

impl Kiro {
    /// Root directory for Kiro **CLI** data.
    ///
    /// This is `kiro-cli`'s own resolver, not a casr convention. From the
    /// bundled TUI in the shipped `kiro-cli-chat` 2.14.2 binary, verbatim:
    ///
    /// ```js
    /// function XTe(){
    ///   let e=process.env.KIRO_HOME;
    ///   if(e&&e.length>0)return e;
    ///   let n=process.env.HOME||process.env.USERPROFILE||WTe();
    ///   return yte(n,".kiro")
    /// }
    /// ```
    ///
    /// Note `KIRO_HOME` replaces the whole root — `.kiro` is *not* appended to
    /// it — which is why this returns it unjoined.
    fn cli_home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("KIRO_HOME") {
            let trimmed = home.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
        dirs::home_dir().map(|h| h.join(".kiro"))
    }

    /// Root directory for Kiro **IDE** data.
    ///
    /// Deliberately does not consult `KIRO_HOME`, because the IDE does not.
    /// `KIRO_HOME` appears zero times in the whole shipped Kiro IDE 1.0.212
    /// package, and all six places the extension builds this path use
    /// `os.homedir()` — e.g. `getDefaultSessionsPath()` in
    /// `src/extension/agent-chat/api/methods/open-file-diff.ts`:
    ///
    /// ```js
    /// return nodePath4.join(os18.homedir(), ".kiro", "sessions");
    /// ```
    ///
    /// The agent's own `sessionsPath` constructor option would override it, but
    /// nothing in the package ever supplies one (`sessionsPath:` occurs zero
    /// times), so the default always wins.
    ///
    /// Honouring `KIRO_HOME` here would point casr at a directory Kiro IDE
    /// never writes to, which is the same defect — reading a path the tool does
    /// not use — that adding this store was meant to fix.
    fn ide_home_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".kiro"))
    }

    /// Directory holding the CLI session triplets.
    fn sessions_dir() -> Option<PathBuf> {
        Self::cli_home_dir().map(|h| h.join("sessions").join("cli"))
    }

    /// Root the Kiro IDE buckets its session directories under.
    fn ide_root() -> Option<PathBuf> {
        Self::ide_home_dir().map(|h| h.join("sessions"))
    }

    /// Every `<bucket>/<sess_id>/session.json` the IDE has written.
    ///
    /// Two levels deep and no deeper: the third level is `tool-outputs/` and
    /// `sub-executions/`, which hold spilled payloads, not sessions.
    fn ide_sessions(unreadable: &mut Vec<UnreadableSource>) -> Vec<(String, PathBuf)> {
        let Some(root) = Self::ide_root() else {
            return vec![];
        };
        let mut out = Vec::new();
        for bucket in read_dir_reporting(&root, unreadable) {
            if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            for session in read_dir_reporting(&bucket.path(), unreadable) {
                if !session.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let meta = session.path().join("session.json");
                if !meta.is_file() {
                    continue;
                }
                let Some(id) = session.file_name().to_str().map(ToString::to_string) else {
                    continue;
                };
                out.push((id, meta));
            }
        }
        out
    }

    /// True when `path` is a Kiro IDE session anchor rather than a CLI one.
    fn is_ide_anchor(path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()) == Some("session.json")
    }

    /// Sibling path for a session file with a different extension.
    fn sibling(path: &Path, ext: &str) -> PathBuf {
        path.with_extension(ext)
    }
}

impl Provider for Kiro {
    fn name(&self) -> &str {
        "Kiro CLI"
    }

    fn slug(&self) -> &str {
        SLUG
    }

    fn cli_alias(&self) -> &str {
        "kr"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        for bin in ["kiro-cli", "kiro"] {
            if which::which(bin).is_ok() {
                evidence.push(format!("{bin} binary found in PATH"));
                installed = true;
                break;
            }
        }

        // Both roots, because they are not always the same directory: with
        // `KIRO_HOME` set they are two, and a user with only the IDE installed
        // has only the second one.
        for home in [Self::cli_home_dir(), Self::ide_home_dir()] {
            if let Some(home) = home
                && home.is_dir()
                && !evidence.contains(&format!("{} exists", home.display()))
            {
                evidence.push(format!("{} exists", home.display()));
                installed = true;
            }
        }

        // Two stores, and neither is the home directory the loop above
        // accepted: the CLI writes `<cli home>/sessions/cli` and the IDE
        // writes `~/.kiro/sessions/<bucket>/<id>/session.json`. An IDE-only
        // install has the second and not the first, which is exactly the case
        // that used to read as "installed, no sessions".
        if installed {
            if let Some(cli) = Self::sessions_dir() {
                evidence.push(store_evidence(&cli));
            }
            if let Some(ide) = Self::ide_root() {
                evidence.push(store_evidence(&ide));
            }
        }

        trace!(provider = SLUG, ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        // `sessions/` rather than `sessions/cli/`: it is the parent of both
        // stores, and every caller uses these roots with `starts_with`.
        match Self::ide_root() {
            Some(dir) if dir.is_dir() => vec![dir],
            _ => vec![],
        }
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        if let Some(dir) = Self::sessions_dir() {
            // The metadata `.json` file is the canonical CLI session anchor.
            let candidate = dir.join(format!("{session_id}.json"));
            if candidate.is_file() {
                return Some(candidate);
            }
            // Tolerate sessions that only ever produced a `.jsonl` journal.
            let jsonl = dir.join(format!("{session_id}.jsonl"));
            if jsonl.is_file() {
                return Some(jsonl);
            }
        }
        // The IDE buckets by a one-way workspace hash, so the bucket holding a
        // given id can only be found by looking in all of them — which is what
        // Kiro's own `deleteSessionAcrossBuckets` does.
        // `owns_session` answers "is this id mine"; a directory it could not
        // read makes the answer `None`, which is what "not mine" already means
        // here, so the failures are collected and dropped rather than reported
        // through a return type that has nowhere to put them.
        Self::ide_sessions(&mut Vec::new())
            .into_iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, path)| path)
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let dir = Self::sessions_dir()?;

        // Anchor on `.json` metadata files; fall back to `.jsonl` for sessions
        // that never wrote metadata. De-dup so a `<id>.json`/`<id>.jsonl` pair
        // counts once.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut listing = SessionListing::default();

        // A missing `sessions/cli` is the right answer for an IDE-only install
        // and is not reported; a `sessions/cli` that exists and cannot be read
        // is, and the IDE scan below still runs either way.
        let paths: Vec<PathBuf> = read_dir_reporting(&dir, &mut listing.unreadable)
            .into_iter()
            .map(|e| e.path())
            .collect();

        let mut sessions: Vec<(String, PathBuf)> = Vec::new();
        let mut push =
            |id: String, path: PathBuf, seen: &mut std::collections::BTreeSet<String>| {
                if seen.insert(id.clone()) {
                    sessions.push((id, path));
                }
            };

        // Two passes so `.json` wins as the anchor path over a bare `.jsonl`.
        for path in paths
            .iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json") && p.is_file())
        {
            if let Some(id) = session_id_from_path(path) {
                push(id, path.clone(), &mut seen);
            }
        }
        for path in paths
            .iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl") && p.is_file())
        {
            if let Some(id) = session_id_from_path(path) {
                push(id, path.clone(), &mut seen);
            }
        }

        // The IDE's store. Same `seen` set, so the session id remains the one
        // key: a session cannot be listed twice because two stores describe it.
        let mut ide_unreadable = Vec::new();
        for (id, path) in Self::ide_sessions(&mut ide_unreadable) {
            push(id, path, &mut seen);
        }

        listing.sessions = sessions;
        listing.unreadable.extend(ide_unreadable);
        Some(listing)
    }

    /// Two stores, two rules. The CLI writes a `<id>.json`/`<id>.jsonl`/
    /// `<id>.history` triplet flat in `sessions/cli`; the IDE writes
    /// `sessions/<bucket>/<sess_id>/session.json`. `.history` is the raw
    /// journal of an already-listed session and `tool-outputs/` below the IDE
    /// anchor holds spilled payloads, so neither is a session of its own.
    fn is_session_path(&self, path: &Path) -> bool {
        if Self::is_ide_anchor(path) {
            return true;
        }
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("json" | "jsonl")
        )
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        if Self::is_ide_anchor(path) {
            return read_ide_session(path);
        }
        debug!(path = %path.display(), "reading Kiro session");

        // The given `path` may be either the `.json` metadata or the `.jsonl`
        // journal; resolve both siblings regardless.
        let json_path = Self::sibling(path, "json");
        let jsonl_path = Self::sibling(path, "jsonl");
        let history_path = Self::sibling(path, "history");

        // --- Metadata (.json) ---------------------------------------------
        let meta: serde_json::Value = if json_path.is_file() {
            let text = std::fs::read_to_string(&json_path)
                .with_context(|| format!("failed to read {}", json_path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("failed to parse JSON {}", json_path.display()))?
        } else {
            serde_json::Value::Null
        };

        let session_id = meta
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| session_id_from_path(path))
            .unwrap_or_else(|| "unknown".to_string());

        let workspace = meta
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);

        let started_at = meta.get("created_at").and_then(parse_timestamp);
        let mut ended_at = meta.get("updated_at").and_then(parse_timestamp);

        // --- Conversation journal (.jsonl) --------------------------------
        let mut messages: Vec<CanonicalMessage> = Vec::new();
        if jsonl_path.is_file() {
            let text = std::fs::read_to_string(&jsonl_path)
                .with_context(|| format!("failed to read {}", jsonl_path.display()))?;
            for (lineno, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let envelope: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        // Tolerate malformed/partial trailing lines.
                        trace!(line = lineno, error = %e, "skipping unparseable Kiro journal line");
                        continue;
                    }
                };
                if let Some(msg) = parse_envelope(&envelope, &mut ended_at) {
                    messages.push(msg);
                }
            }
        }

        reindex_messages(&mut messages);

        // --- Title --------------------------------------------------------
        let title = meta
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| truncate_title(s, 100))
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });

        // --- Model name ---------------------------------------------------
        // Kiro records the model on each `thinking` part (`modelId`) and may
        // also carry it in `session_state.rts_model_state.model_info`.
        let model_name = messages
            .iter()
            .filter_map(|m| m.author.as_deref())
            .find(|a| !a.is_empty() && *a != "user" && *a != "reasoning")
            .map(String::from)
            .or_else(|| {
                meta.pointer("/session_state/rts_model_state/model_info")
                    .and_then(model_name_from_info)
            });

        // --- History (.history) -------------------------------------------
        let history = if history_path.is_file() {
            std::fs::read_to_string(&history_path).ok()
        } else {
            None
        };

        // --- Metadata bag (preserved for round-trip fidelity) -------------
        let mut metadata = serde_json::Map::new();
        metadata.insert("source".into(), serde_json::Value::String(SLUG.to_string()));
        if let Some(state) = meta.get("session_state") {
            metadata.insert("session_state".into(), state.clone());
        }
        for key in ["parent_session_id", "session_created_reason"] {
            if let Some(v) = meta.get(key)
                && !v.is_null()
            {
                metadata.insert(key.into(), v.clone());
            }
        }
        if let Some(h) = &history {
            metadata.insert("history".into(), serde_json::Value::String(h.clone()));
        }

        debug!(session_id, messages = messages.len(), "Kiro session parsed");

        Ok(CanonicalSession {
            session_id,
            provider_slug: SLUG.to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: if json_path.is_file() {
                json_path
            } else {
                jsonl_path
            },
            model_name,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let target_session_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();

        let dir = Self::sessions_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Kiro sessions directory"))?;
        let json_path = dir.join(format!("{target_session_id}.json"));
        let jsonl_path = dir.join(format!("{target_session_id}.jsonl"));
        let history_path = dir.join(format!("{target_session_id}.history"));

        debug!(
            target_session_id,
            json = %json_path.display(),
            "writing Kiro session"
        );

        // --- Metadata (.json) ---------------------------------------------
        let created_at = session
            .started_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(now);
        let updated_at = session
            .ended_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(now);

        let mut meta = serde_json::Map::new();
        meta.insert(
            "session_id".into(),
            serde_json::Value::String(target_session_id.clone()),
        );
        meta.insert(
            "cwd".into(),
            serde_json::Value::String(
                session
                    .workspace
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        );
        meta.insert(
            "created_at".into(),
            serde_json::Value::String(rfc3339_micros(created_at)),
        );
        meta.insert(
            "updated_at".into(),
            serde_json::Value::String(rfc3339_micros(updated_at)),
        );
        meta.insert(
            "title".into(),
            serde_json::Value::String(session.title.clone().unwrap_or_default()),
        );
        // Preserve parent/reason/session_state from the canonical metadata bag
        // when present so a Kiro→…→Kiro round-trip keeps the nested state.
        meta.insert(
            "parent_session_id".into(),
            session
                .metadata
                .get("parent_session_id")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
        meta.insert(
            "session_created_reason".into(),
            session
                .metadata
                .get("session_created_reason")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
        let session_state = session
            .metadata
            .get("session_state")
            .cloned()
            .unwrap_or_else(|| default_session_state(&target_session_id, session));
        meta.insert("session_state".into(), session_state);

        let json_bytes =
            serde_json::to_string_pretty(&serde_json::Value::Object(meta))?.into_bytes();

        // --- Conversation journal (.jsonl) --------------------------------
        let mut jsonl = String::new();
        for msg in &session.messages {
            if let Some(envelope) = message_to_envelope(msg) {
                jsonl.push_str(&serde_json::to_string(&envelope)?);
                jsonl.push('\n');
            }
        }

        // --- Write all files atomically -----------------------------------
        //
        // Three files, so three backups: reporting only the first one's meant a
        // rolled-back `--force` write left the journal's and the history's
        // predecessors in `.bak` files that nothing would ever put back.
        let mut written_paths = Vec::new();
        let mut backups = Vec::new();

        let json_outcome =
            crate::pipeline::atomic_write(&json_path, &json_bytes, opts.force, self.slug())?;
        backups.extend(json_outcome.displaced());
        written_paths.push(json_outcome.target_path);

        let jsonl_outcome =
            crate::pipeline::atomic_write(&jsonl_path, jsonl.as_bytes(), opts.force, self.slug())?;
        backups.extend(jsonl_outcome.displaced());
        written_paths.push(jsonl_outcome.target_path);

        // `.history` is optional; only emit when we carried one through.
        if let Some(history) = session.metadata.get("history").and_then(|v| v.as_str()) {
            let hist_outcome = crate::pipeline::atomic_write(
                &history_path,
                history.as_bytes(),
                opts.force,
                self.slug(),
            )?;
            backups.extend(hist_outcome.displaced());
            written_paths.push(hist_outcome.target_path);
        }

        info!(
            target_session_id,
            files = written_paths.len(),
            messages = session.messages.len(),
            "Kiro session written"
        );

        Ok(WrittenSession {
            paths: written_paths,
            session_id: target_session_id.clone(),
            resume_command: self.resume_command(&target_session_id),
            backups,
            warnings: Vec::new(),
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("kiro-cli --resume-id {session_id}")
    }
}

// ---------------------------------------------------------------------------
// Reader helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Kiro IDE reader
// ---------------------------------------------------------------------------

/// Read one `sess_<uuid>/session.json` + its `messages.jsonl` sibling.
fn read_ide_session(meta_path: &Path) -> anyhow::Result<CanonicalSession> {
    debug!(path = %meta_path.display(), "reading Kiro IDE session");

    let text = std::fs::read_to_string(meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let meta: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse JSON {}", meta_path.display()))?;

    let dir = meta_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", meta_path.display()))?;

    let session_id = meta
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    // The bucket directory is a one-way hash, so `workspacePaths` is the only
    // source. An empty array is Kiro's own "_global" case: no workspace, which
    // is reported as such rather than guessed at.
    let workspace = meta
        .get("workspacePaths")
        .and_then(|v| v.as_array())
        .and_then(|paths| paths.first())
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    let started_at = meta.get("createdAt").and_then(parse_timestamp);
    let mut ended_at = meta.get("lastModifiedAt").and_then(parse_timestamp);

    let mut messages: Vec<CanonicalMessage> = Vec::new();
    let jsonl_path = dir.join("messages.jsonl");
    if jsonl_path.is_file() {
        let text = std::fs::read_to_string(&jsonl_path)
            .with_context(|| format!("failed to read {}", jsonl_path.display()))?;
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    // A crashed IDE leaves a half-written trailing line; the
                    // rest of the transcript is still good.
                    trace!(line = lineno, error = %e, "skipping unparseable Kiro IDE message");
                    continue;
                }
            };
            if let Some(msg) = parse_ide_message(&record, &mut ended_at) {
                messages.push(msg);
            }
        }
    }

    reindex_messages(&mut messages);

    let title = meta
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| truncate_title(s, 100))
        .or_else(|| {
            messages
                .iter()
                .find(|m| m.role == MessageRole::User)
                .map(|m| truncate_title(&m.content, 100))
        });

    let model_name = meta
        .get("modelId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);

    // Preserve everything the canonical fields cannot carry, so an IDE session
    // that leaves and comes back does not lose its mode or its lineage.
    let mut metadata = serde_json::Map::new();
    metadata.insert("source".into(), serde_json::Value::String(SLUG.to_string()));
    metadata.insert(
        "kiro_store".into(),
        serde_json::Value::String("ide".to_string()),
    );
    for key in [
        "agentMode",
        "parentSessionId",
        "parentExecutionId",
        "createdReason",
        "executionTarget",
        "repositories",
        "schemaVersion",
        "dataModelVersion",
        "workspacePaths",
    ] {
        if let Some(v) = meta.get(key)
            && !v.is_null()
        {
            metadata.insert(key.into(), v.clone());
        }
    }

    debug!(
        session_id,
        messages = messages.len(),
        "Kiro IDE session parsed"
    );

    Ok(CanonicalSession {
        session_id,
        provider_slug: SLUG.to_string(),
        workspace,
        title,
        started_at,
        ended_at,
        messages,
        metadata: serde_json::Value::Object(metadata),
        source_path: meta_path.to_path_buf(),
        model_name,
    })
}

/// Parse one `{"id","timestamp","payload"}` line of `messages.jsonl`.
///
/// Returns `None` for the lifecycle events (`turn_start`, `tombstone`,
/// `usage_summary`, …) that share the file with the conversation but are not
/// messages.
fn parse_ide_message(
    record: &serde_json::Value,
    ended_at: &mut Option<i64>,
) -> Option<CanonicalMessage> {
    let payload = record.get("payload")?;
    let kind = payload.get("type").and_then(|v| v.as_str())?;

    let timestamp = record.get("timestamp").and_then(parse_timestamp);
    if let Some(t) = timestamp {
        *ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
    }

    let text = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    let (role, content, author, tool_calls, tool_results) = match kind {
        "user" => (
            MessageRole::User,
            text("content"),
            Some("user".to_string()),
            Vec::new(),
            Vec::new(),
        ),
        "assistant" => (
            MessageRole::Assistant,
            text("content"),
            // `operationType: "Reasoning"` is how the IDE marks thinking; the
            // rest of this codebase spells that author `"reasoning"`.
            match payload.get("operationType").and_then(|v| v.as_str()) {
                Some("Reasoning") => Some("reasoning".to_string()),
                _ => payload
                    .get("reasoningModelId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
            },
            Vec::new(),
            Vec::new(),
        ),
        "tool_call" => (
            MessageRole::Assistant,
            String::new(),
            None,
            vec![ToolCall {
                id: payload
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                name: payload
                    .get("toolName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: payload
                    .get("args")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            }],
            Vec::new(),
        ),
        "tool_result" => (
            MessageRole::Tool,
            String::new(),
            None,
            Vec::new(),
            vec![ToolResult {
                call_id: payload
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                content: text("content"),
                is_error: payload
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .map(|ok| !ok)
                    .unwrap_or(false),
            }],
        ),
        "system" | "agent_note" => (
            MessageRole::System,
            text("content"),
            None,
            Vec::new(),
            Vec::new(),
        ),
        "error" => (
            MessageRole::System,
            text("message"),
            None,
            Vec::new(),
            Vec::new(),
        ),
        // Lifecycle events, not conversation.
        _ => return None,
    };

    if content.trim().is_empty() && tool_calls.is_empty() && tool_results.is_empty() {
        return None;
    }

    Some(CanonicalMessage {
        idx: 0,
        role,
        content,
        timestamp,
        author,
        tool_calls,
        tool_results,
        // The whole record, so a round-trip keeps the fields the canonical
        // message has nowhere to put (status, executionId, snapshot ids, …).
        extra: record.clone(),
    })
}

/// Extract the session id from a `<id>.{json,jsonl,history}` path's file stem.
fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(ToString::to_string)
        .filter(|s| !s.is_empty())
}

/// Parse a single `.jsonl` envelope into a canonical message.
///
/// Returns `None` for unknown `kind`s or envelopes that carry no content.
fn parse_envelope(
    envelope: &serde_json::Value,
    ended_at: &mut Option<i64>,
) -> Option<CanonicalMessage> {
    let kind = envelope.get("kind").and_then(|v| v.as_str())?;
    let data = envelope.get("data").unwrap_or(&serde_json::Value::Null);

    let role = match kind {
        "Prompt" => MessageRole::User,
        "AssistantMessage" => MessageRole::Assistant,
        "ToolResults" => MessageRole::Tool,
        // Unknown envelope kinds are preserved as `Other` rather than dropped,
        // so future Kiro additions degrade gracefully instead of vanishing.
        other => MessageRole::Other(other.to_string()),
    };

    // Per-message timestamp lives at `data.meta.timestamp` (epoch seconds) on
    // Prompt envelopes; other kinds may omit it.
    let timestamp = data.pointer("/meta/timestamp").and_then(parse_timestamp);
    if let Some(t) = timestamp {
        *ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
    }

    let content_parts = data
        .get("content")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_results: Vec<ToolResult> = Vec::new();
    let mut author: Option<String> = None;

    for part in &content_parts {
        let Some(part_kind) = part.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let pdata = part.get("data").unwrap_or(&serde_json::Value::Null);
        match part_kind {
            // `text` parts carry the string directly under `data`.
            "text" => {
                if let Some(s) = pdata.as_str()
                    && !s.is_empty()
                {
                    text_chunks.push(s.to_string());
                }
            }
            // `thinking` parts carry `{ modelId, text, signature, ... }`.
            "thinking" => {
                if author.is_none()
                    && let Some(m) = pdata.get("modelId").and_then(|v| v.as_str())
                    && !m.is_empty()
                {
                    author = Some(m.to_string());
                }
                // Reasoning text is preserved (kept distinct from prose by the
                // round-trip via the `extra` bag below).
                if let Some(s) = pdata.get("text").and_then(|v| v.as_str())
                    && !s.trim().is_empty()
                {
                    text_chunks.push(s.to_string());
                }
            }
            // `toolUse` → `{ toolUseId, name, input }`.
            "toolUse" => {
                tool_calls.push(ToolCall {
                    id: pdata
                        .get("toolUseId")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                    name: pdata
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    arguments: pdata
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                });
            }
            // `toolResult` → `{ toolUseId, content: [...], status }`.
            "toolResult" => {
                tool_results.push(ToolResult {
                    call_id: pdata
                        .get("toolUseId")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                    content: tool_result_text(pdata.get("content")),
                    is_error: pdata
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s.eq_ignore_ascii_case("error"))
                        .unwrap_or(false),
                });
            }
            _ => {}
        }
    }

    if text_chunks.is_empty() && tool_calls.is_empty() && tool_results.is_empty() {
        return None;
    }

    Some(CanonicalMessage {
        idx: 0,
        role,
        content: text_chunks.join("\n\n"),
        timestamp,
        author: author.or_else(|| match kind {
            "Prompt" => Some("user".to_string()),
            _ => None,
        }),
        tool_calls,
        tool_results,
        // Preserve the full envelope for high-fidelity round-trip (the nested
        // `results` map on ToolResults can't be reconstructed from the
        // canonical fields alone).
        extra: envelope.clone(),
    })
}

/// Flatten a Kiro `toolResult.content` array (`[{kind:"json"|"text", data:…}]`)
/// into a single string.
fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    let Some(serde_json::Value::Array(parts)) = content else {
        return content.map(stringify_value).unwrap_or_default();
    };
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        let pdata = part.get("data");
        match part.get("kind").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(s) = pdata.and_then(|v| v.as_str()) {
                    out.push(s.to_string());
                } else if let Some(d) = pdata {
                    out.push(stringify_value(d));
                }
            }
            // `json` results (and any other kind): prefer stdout when present,
            // else serialize the whole payload.
            _ => {
                if let Some(d) = pdata {
                    if let Some(stdout) = d.get("stdout").and_then(|v| v.as_str()) {
                        let mut chunk = stdout.to_string();
                        if let Some(stderr) = d.get("stderr").and_then(|v| v.as_str())
                            && !stderr.is_empty()
                        {
                            chunk.push('\n');
                            chunk.push_str(stderr);
                        }
                        out.push(chunk);
                    } else {
                        out.push(stringify_value(d));
                    }
                }
            }
        }
    }
    out.join("\n")
}

/// Stringify an arbitrary JSON value: strings as-is, everything else serialized.
fn stringify_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Best-effort extraction of a model name from `rts_model_state.model_info`,
/// which Kiro leaves `null` in many sessions.
fn model_name_from_info(info: &serde_json::Value) -> Option<String> {
    match info {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("model_id")
            .or_else(|| obj.get("modelId"))
            .or_else(|| obj.get("name"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Writer helpers
// ---------------------------------------------------------------------------

/// Render a UTC timestamp in Kiro's observed `...Z` micros format.
fn rfc3339_micros(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Build a minimal but well-formed `session_state` when none was carried.
fn default_session_state(session_id: &str, session: &CanonicalSession) -> serde_json::Value {
    let cwd = session
        .workspace
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let allowed_read = cwd
        .as_ref()
        .map(|c| serde_json::json!([c]))
        .unwrap_or_else(|| serde_json::json!([]));
    serde_json::json!({
        "version": "v1",
        "conversation_metadata": {
            "user_turn_metadatas": [],
            "user_turn_start_request": null,
            "last_request": null
        },
        "rts_model_state": {
            "conversation_id": session_id,
            "model_info": null,
            "context_usage_percentage": null
        },
        "permissions": {
            "filesystem": {
                "allowed_read_paths": allowed_read,
                "allowed_write_paths": [],
                "denied_read_paths": [],
                "denied_write_paths": []
            },
            "trusted_tools": [],
            "denied_tools": [],
            "allowed_commands": []
        },
        "agent_name": null
    })
}

/// Serialize a canonical message into a Kiro `.jsonl` envelope.
///
/// When the message still carries its original Kiro envelope in `extra`, we
/// re-emit it verbatim for maximal round-trip fidelity. Otherwise we
/// synthesize an envelope from the canonical fields (cross-provider import).
fn message_to_envelope(msg: &CanonicalMessage) -> Option<serde_json::Value> {
    if let Some(kind) = msg.extra.get("kind").and_then(|v| v.as_str())
        && matches!(kind, "Prompt" | "AssistantMessage" | "ToolResults")
        && msg.extra.get("data").is_some()
    {
        return Some(msg.extra.clone());
    }

    // Synthesize for messages that did not originate from Kiro.
    let kind = match msg.role {
        MessageRole::User | MessageRole::System => "Prompt",
        MessageRole::Assistant => "AssistantMessage",
        MessageRole::Tool => "ToolResults",
        MessageRole::Other(_) => "AssistantMessage",
    };

    let mut content: Vec<serde_json::Value> = Vec::new();
    if !msg.content.is_empty() {
        content.push(serde_json::json!({ "kind": "text", "data": msg.content }));
    }
    for tc in &msg.tool_calls {
        content.push(serde_json::json!({
            "kind": "toolUse",
            "data": {
                "toolUseId": tc.id.clone().unwrap_or_default(),
                "name": tc.name,
                "input": tc.arguments,
            }
        }));
    }
    for tr in &msg.tool_results {
        content.push(serde_json::json!({
            "kind": "toolResult",
            "data": {
                "toolUseId": tr.call_id.clone().unwrap_or_default(),
                "content": [{ "kind": "text", "data": tr.content }],
                "status": if tr.is_error { "error" } else { "success" },
            }
        }));
    }

    if content.is_empty() {
        return None;
    }

    let message_id = uuid::Uuid::new_v4().to_string();
    let mut data = serde_json::Map::new();
    data.insert("message_id".into(), serde_json::Value::String(message_id));
    data.insert("content".into(), serde_json::Value::Array(content));
    if kind == "Prompt"
        && let Some(ts) = msg.timestamp
    {
        data.insert("meta".into(), serde_json::json!({ "timestamp": ts / 1000 }));
    }

    Some(serde_json::json!({
        "version": "v1",
        "kind": kind,
        "data": serde_json::Value::Object(data),
    }))
}

#[cfg(test)]
mod tests {
    // NOTE: `src/lib.rs` declares `#![forbid(unsafe_code)]`, so these in-crate
    // unit tests must avoid mutating the process environment (`set_var` is
    // `unsafe` in edition 2024). Env-dependent round-trip + CLI smoke coverage
    // lives in `tests/kiro_test.rs`, which is a separate crate and may use the
    // shared `EnvGuard`/`EnvLock` harness.
    use super::*;
    use crate::model::{CanonicalMessage, MessageRole};
    use std::io::Write as _;

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kiro");
    const FIXTURE_ID: &str = "0a5376f2-7e2f-4981-bcbc-67195586604a";

    fn fixture_json_path() -> PathBuf {
        PathBuf::from(FIXTURE_DIR).join(format!("{FIXTURE_ID}.json"))
    }

    // -----------------------------------------------------------------------
    // Trait surface
    // -----------------------------------------------------------------------

    #[test]
    fn slug_and_alias() {
        let p = Kiro;
        assert_eq!(p.slug(), "kiro");
        assert_eq!(p.cli_alias(), "kr");
        assert_eq!(p.name(), "Kiro CLI");
    }

    #[test]
    fn resume_command_uses_resume_id_flag() {
        assert_eq!(
            Kiro.resume_command("abc-123"),
            "kiro-cli --resume-id abc-123"
        );
    }

    #[test]
    fn sibling_swaps_extension() {
        let p = Path::new("/x/sessions/cli/abc.json");
        assert_eq!(
            Kiro::sibling(p, "jsonl"),
            Path::new("/x/sessions/cli/abc.jsonl")
        );
        assert_eq!(
            Kiro::sibling(p, "history"),
            Path::new("/x/sessions/cli/abc.history")
        );
    }

    // -----------------------------------------------------------------------
    // Reading the REAL captured fixture (absolute path; no env mutation)
    // -----------------------------------------------------------------------

    #[test]
    fn reads_real_fixture_metadata_and_messages() {
        let session = Kiro
            .read_session(&fixture_json_path())
            .expect("read real Kiro fixture");

        assert_eq!(session.session_id, FIXTURE_ID);
        assert_eq!(session.provider_slug, "kiro");
        assert_eq!(
            session
                .workspace
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned()),
            Some(
                "/Users/tranquangdang21/Projects/jcode/.worktrees/feat-380-compaction-resistant-notepad"
                    .to_string()
            )
        );
        assert!(session.started_at.is_some());
        assert!(session.ended_at.is_some());

        // Prompt → User, AssistantMessage → Assistant, ToolResults → Tool.
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[2].role, MessageRole::Tool);

        assert!(
            session.messages[0]
                .content
                .contains("Research ONLY the repo")
        );

        // Assistant turn carries tool calls + a model id from `thinking`.
        assert!(!session.messages[1].tool_calls.is_empty());
        assert_eq!(session.model_name.as_deref(), Some("claude-opus-4.8"));

        // ToolResults turn surfaces tool output (stdout flattened).
        assert!(!session.messages[2].tool_results.is_empty());
        assert!(
            session.messages[2]
                .tool_results
                .iter()
                .any(|r| r.content.contains("origin"))
        );

        // Nested session_state + parent linkage preserved.
        assert!(session.metadata.get("session_state").is_some());
        assert_eq!(
            session
                .metadata
                .get("parent_session_id")
                .and_then(|v| v.as_str()),
            Some("98cb06e6-28da-4ba8-8ebe-be6bf16841c1")
        );

        // The `.history` plain-text sidecar is captured.
        let history = session
            .metadata
            .get("history")
            .and_then(|v| v.as_str())
            .expect("history captured");
        assert!(history.contains("/model"));
        assert!(history.contains("/exit"));
    }

    // -----------------------------------------------------------------------
    // Round-trip at the serialization layer (no filesystem / env needed):
    // real fixture → canonical → re-emit envelopes → re-parse equals.
    // -----------------------------------------------------------------------

    #[test]
    fn envelope_round_trip_preserves_messages() {
        let original = Kiro
            .read_session(&fixture_json_path())
            .expect("read original");

        // Re-emit each message to a Kiro envelope, then re-parse it.
        let mut ended = None;
        let reparsed: Vec<_> = original
            .messages
            .iter()
            .map(|m| {
                let env = message_to_envelope(m).expect("envelope for non-empty message");
                parse_envelope(&env, &mut ended).expect("re-parse envelope")
            })
            .collect();

        assert_eq!(reparsed.len(), original.messages.len());
        for (a, b) in original.messages.iter().zip(reparsed.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content, "content drift at idx {}", a.idx);
            assert_eq!(a.tool_calls.len(), b.tool_calls.len());
            assert_eq!(a.tool_results.len(), b.tool_results.len());
        }
    }

    // -----------------------------------------------------------------------
    // Synthesizing envelopes for foreign (non-Kiro) sessions.
    // -----------------------------------------------------------------------

    #[test]
    fn synthesizes_envelopes_for_foreign_messages() {
        let user = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Hi there".into(),
            timestamp: Some(1_700_000_000_000),
            author: Some("user".into()),
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };
        let assistant = CanonicalMessage {
            idx: 1,
            role: MessageRole::Assistant,
            content: "Hello back".into(),
            timestamp: None,
            author: None,
            tool_calls: vec![ToolCall {
                id: Some("t1".into()),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("t1".into()),
                content: "file.txt".into(),
                is_error: false,
            }],
            extra: serde_json::Value::Null,
        };

        let u_env = message_to_envelope(&user).unwrap();
        assert_eq!(u_env["kind"], "Prompt");
        // Prompt timestamps are emitted as epoch seconds under data.meta.
        assert_eq!(u_env["data"]["meta"]["timestamp"], 1_700_000_000);

        let a_env = message_to_envelope(&assistant).unwrap();
        assert_eq!(a_env["kind"], "AssistantMessage");

        let mut ended = None;
        let ru = parse_envelope(&u_env, &mut ended).unwrap();
        let ra = parse_envelope(&a_env, &mut ended).unwrap();
        assert_eq!(ru.content, "Hi there");
        assert_eq!(ra.content, "Hello back");
        assert_eq!(ra.tool_calls.len(), 1);
        assert_eq!(ra.tool_calls[0].name, "shell");
        assert_eq!(ra.tool_results.len(), 1);
        assert_eq!(ra.tool_results[0].content, "file.txt");
    }

    // -----------------------------------------------------------------------
    // Robustness
    // -----------------------------------------------------------------------

    #[test]
    fn tolerates_unknown_kinds_and_malformed_lines() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            tmp,
            r#"{{"version":"v1","kind":"Prompt","data":{{"content":[{{"kind":"text","data":"hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(tmp, "this is not json at all").unwrap();
        writeln!(
            tmp,
            r#"{{"version":"v1","kind":"SomethingNew","data":{{"content":[{{"kind":"text","data":"future"}}]}}}}"#
        )
        .unwrap();
        tmp.flush().unwrap();

        let session = Kiro.read_session(tmp.path()).expect("tolerant read");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert!(matches!(session.messages[1].role, MessageRole::Other(_)));
    }

    #[test]
    fn empty_journal_yields_no_messages() {
        let tmp = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let session = Kiro.read_session(tmp.path()).expect("read empty");
        assert_eq!(session.messages.len(), 0);
    }

    #[test]
    fn tool_result_text_flattens_json_stdout() {
        let content = serde_json::json!([
            {"kind": "json", "data": {"stdout": "out", "stderr": "err", "exit_status": "exit status: 0"}}
        ]);
        assert_eq!(tool_result_text(Some(&content)), "out\nerr");
    }
}
