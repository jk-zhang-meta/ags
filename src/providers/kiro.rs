//! Kiro provider — reads/writes both session layouts Kiro keeps under `~/.kiro`.
//!
//! Kiro ships as two products that share one home directory, and they keep two
//! session *layouts* between them. `detect()` fires on `~/.kiro` existing,
//! which either product creates, so reading only one layout renders as
//! "installed, 0 sessions" for every user of the other — indistinguishable
//! from having none.
//!
//! ```text
//! <root>/sessions/cli/<uuid>.{json,jsonl,history}   ← flat: kiro-cli's classic/v2 store
//! <root>/sessions/<workspaceHash>/<id>/…            ← bucketed: the shared "KAS" store
//! ```
//!
//! # Two layouts, not two products
//!
//! The bucketed layout is *not* the IDE's private store. `kiro-cli` 2.14.2
//! reads **and writes** it too, under its own root. Measured against the
//! shipped `kiro-cli-chat` 2.14.2 binary (`BUILD_VERSION=2.14.2`), whose
//! internal wire surface `chat _ ensure-session` is what `--resume-id` is
//! implemented in terms of:
//!
//! ```text
//! $ KIRO_HOME=…/khome3 kiro-cli-chat chat _ ensure-session \
//!       --source-format v2 --source-session-id <uuid> --target-format kas --cwd /tmp/wsX
//! {"kind":"ensureSession","data":{"sessionId":"cli_<uuid>_uHORXqEL"}}
//! $ find "$KIRO_HOME/sessions"
//!   …/khome3/sessions/c25a05601239adfe/cli_<uuid>_uHORXqEL/session.json
//!   …/khome3/sessions/c25a05601239adfe/cli_<uuid>_uHORXqEL/messages.jsonl
//! $ printf '/tmp/wsX' | sha256sum | cut -c1-16   →  c25a05601239adfe
//! ```
//!
//! That is the same bucket rule, the same two filenames and the same root the
//! IDE uses. In the binary it is `chat_cli_v2::agent::kas::persist::
//! write_kas_session_dir` rooted at `chat_cli_v2::agent::kas::shared::
//! default_kas_sessions_root`, which is `kiro_home_dir_from_process_env()`
//! joined with `"sessions"`. So a bucketed session says nothing about which
//! product wrote it, and neither does its id: `kiro-cli` mints `sess_<uuid>`
//! natively ("V2 uses a UUID; KAS uses the `sess_<uuid>` form" — its own
//! `--source-session-id` help) and `cli_<uuid>` when converting or importing.
//!
//! # The two roots are not the same variable
//!
//! Both layouts hang off `<root>/sessions`, and each product resolves `<root>`
//! by its own rule:
//!
//! * `kiro-cli` uses `$KIRO_HOME` when that is set and non-empty, else
//!   `~/.kiro` — see [`Kiro::cli_home_dir`], which quotes it. `KIRO_HOME`
//!   *replaces* the root rather than being a parent to append `.kiro` to.
//! * The Kiro IDE is always `~/.kiro`. It has no relocation variable at all:
//!   `KIRO_HOME` occurs zero times in the entire shipped package. See
//!   [`Kiro::ide_home_dir`].
//!
//! So `KIRO_HOME=/tmp/x` moves kiro-cli's sessions — *both* its layouts — and
//! leaves the IDE's where they were. casr therefore scans the bucketed layout
//! under **both** roots ([`Kiro::kas_roots`]) and the flat layout under the
//! CLI's root. Pinning the bucketed scan to `~/.kiro` alone, on the theory
//! that bucketed means IDE, silently drops every kiro-cli session written
//! while `KIRO_HOME` was set.
//!
//! One real CLI variable is deliberately not read: `KIRO_TEST_SESSIONS_DIR`,
//! which replaces `sessions/cli` outright. It is part of the CLI's `KIRO_TEST_*`
//! harness family, not a user-facing relocation knob.
//!
//! # The flat layout (`kiro-cli`'s classic/v2 store)
//!
//! AWS/Amazon's agentic coding CLI, backed by Amazon Bedrock. Each session is
//! up to three sibling files keyed by the session UUID:
//!
//! ```text
//! <cliRoot>/sessions/cli/<id>.json      ← session metadata + nested session_state
//! <cliRoot>/sessions/cli/<id>.jsonl     ← append-only conversation journal
//! <cliRoot>/sessions/cli/<id>.history   ← raw prompt input, last 100 lines (optional, not read)
//! ```
//!
//! # The bucketed layout (the shared "KAS" store)
//!
//! Written by the IDE's bundled `kiro.kiro-agent` extension and by `kiro-cli`
//! alike. A session is a *directory* under a per-workspace bucket:
//!
//! ```text
//! <root>/sessions/<bucket>/<id>/session.json      ← metadata
//! <root>/sessions/<bucket>/<id>/messages.jsonl    ← one JSON event per line
//! <root>/sessions/<bucket>/<id>/tool-outputs/…    ← spilled tool output
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
//! `assistant`, `tool_call` and `tool_result`. `session_start` carries a fifth
//! — see below. The rest are session lifecycle events (`turn_start`,
//! `tombstone`, `usage_summary`, …) and are not messages.
//!
//! ## `session_start` is the opening user turn
//!
//! It looks like a lifecycle marker and is not one. In the shipped
//! extension.js the payload is built from the session's *first* prompt —
//! `content` is the prompt's text entries joined — and written exactly once,
//! only on the first turn:
//!
//! ```js
//! c36 = { type: "session_start", agentType: t16, content: a36, …,
//!         forcedRole: s27.forcedRole, messageId: s27.messageId };
//! // Se17: return t16 ? i30 ? Z22(n29) ? { write: false } : { write: true, artifact: V19(…) } …
//! ```
//!
//! No `user` payload is written for that turn, and Kiro's own model-context
//! rebuild replays it as a human message before anything else:
//!
//! ```js
//! for (const h52 of s27) if (h52.payload.type === "session_start") {
//!   n29.push(ke19(h52.payload)); break;   // ke19: pt3.fromHuman(messageId).withText(content)
//! }
//! ```
//!
//! Discarding it therefore loses the prompt that started the session. (Kiro's
//! *other* projection — the one that builds UI items — does return `[]` for
//! `session_start`, alongside `system`/`agent_note`/`error`; that one is not
//! the conversation.)
//!
//! ## Why the two scans cannot collide
//!
//! The two layouts share a parent, always: `<root>/sessions` holds both
//! `cli/` and the buckets. The bucketed scan looks for
//! `<sessions>/<dir>/<dir>/session.json`. `sessions/cli` *is* a bucket-shaped
//! directory, but its children are files, not directories, so it yields
//! nothing — and a bucket name is 16 hex characters or `_global`, never `cli`.
//! `list_sessions` de-duplicates on the session id, which is the only key both
//! layouts agree on, and which also collapses the two roots when they coincide.
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
//!   "imported_from": …,
//!   "session_state": { "version": "v1", "conversation_metadata": { … },
//!                      "rts_model_state": { … }, "permissions": { … },
//!                      "agent_name": …, "goal": … }
//! }
//! ```
//!
//! ## `.jsonl` (conversation journal)
//!
//! Each line is a versioned envelope `{"version":"v1","kind":<Kind>,"data":{…}}`
//! carrying the adjacently tagged `LogEntryV1`, whose five variants the shipped
//! `kiro-cli-chat` 2.14.2 binary names in its serde strings: `Prompt` (user),
//! `AssistantMessage` (assistant), `ToolResults` (tool), `Compaction`
//! (`summary` + `messages_snapshot`) and `ResetTo` (`target_index`). The last
//! two carry no turn and no `content`, so [`parse_envelope`] yields nothing for
//! them. There is no system or operator member, which is what
//! [`message_to_envelopes`] has to place a system turn against. The
//! `data.content` array carries typed parts whose
//! own `kind` is `text` | `thinking` | `toolUse` | `toolResult`:
//!
//! - `text`     → `data` is a plain string.
//! - `thinking` → `data` is `{ modelId, text, signature, redactedContent }`.
//! - `toolUse`  → `data` is `{ toolUseId, name, input }`.
//! - `toolResult` → `data` is `{ toolUseId, content: [...], status }`.
//!
//! A `ToolResults` line additionally carries `data.results`, a map keyed by
//! tool-use id with the typed invocation/outcome Kiro actually replays. The
//! reader derives canonical tool results from the bounded `data.content`
//! blocks; the writer reconstructs the minimum accepted `results` entries and
//! never republishes the unbounded vendor map from `extra`.
//!
//! ## Resume
//!
//! One flag, two very different contracts — see [`Kiro::resume_command`].
//!
//! ```bash
//! kiro-cli --resume-id <uuid>                          # flat layout, from anywhere
//! cd <workspace> && kiro-cli --v3 --resume-id <id>     # bucketed layout
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
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

    /// Directory holding the flat CLI session triplets.
    fn sessions_dir() -> Option<PathBuf> {
        Self::cli_home_dir().map(|h| h.join("sessions").join("cli"))
    }

    /// Every `<root>/sessions` the bucketed layout can live under.
    ///
    /// Both, because both products write it and they resolve `<root>`
    /// differently: `kiro-cli` honours `KIRO_HOME`, the IDE does not. With
    /// `KIRO_HOME` unset the two coincide and this collapses to one entry, so
    /// the scan below never sees the same directory twice.
    fn kas_roots() -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for home in [Self::cli_home_dir(), Self::ide_home_dir()] {
            if let Some(root) = home.map(|h| h.join("sessions"))
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
        roots
    }

    /// Every `<bucket>/<id>/session.json` in the bucketed layout, under every
    /// root it can live under.
    ///
    /// Two levels deep and no deeper: the third level is `tool-outputs/` and
    /// `sub-executions/`, which hold spilled payloads, not sessions.
    fn kas_sessions(unreadable: &mut Vec<UnreadableSource>) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for root in Self::kas_roots() {
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
        }
        out
    }

    /// True when `path` anchors a bucketed session rather than a flat one.
    fn is_kas_anchor(path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()) == Some("session.json")
    }

    /// Which resume contract a given id falls under.
    ///
    /// Decided by looking the id up rather than by its prefix. The prefix used
    /// to be read as "`sess_` means IDE", and that is not a fact about the id:
    /// `kiro-cli` mints `sess_` ids for its own bucketed sessions and `cli_`
    /// ids for converted ones. Where the session actually sits on disk is the
    /// thing that decides how it is resumed, so that is what gets asked.
    fn resume_form(session_id: &str) -> ResumeForm {
        let Some(anchor) = Self::kas_sessions(&mut Vec::new())
            .into_iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, path)| path)
        else {
            return ResumeForm::Flat;
        };
        // The bucket directory is a one-way hash, so `workspacePaths` is the
        // only source for the directory `--resume-id` has to be run from.
        let workspace = std::fs::read_to_string(&anchor)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|meta| {
                meta.get("workspacePaths")?
                    .as_array()?
                    .first()?
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
            });
        match workspace {
            Some(dir) => ResumeForm::Bucketed(dir),
            None => ResumeForm::Unreachable,
        }
    }

    /// Sibling path for a session file with a different extension.
    fn sibling(path: &Path, ext: &str) -> PathBuf {
        path.with_extension(ext)
    }

    /// Reduce `session_state` to the only value casr can safely publish.
    ///
    /// `conversation_metadata` contains outbound model requests,
    /// `rts_model_state` has an open extension bag, and `permissions` is itself
    /// nested. Copying any parent object is therefore still a wholesale vendor
    /// blob. The model name is derived before this filter and parent linkage is
    /// a separate top-level field, so no displayed fact depends on those
    /// objects.
    ///
    /// The shipped 2.14.2 loader accepts an empty or version-only state, and
    /// Kiro's own export omits the state while its import accepts that export.
    /// Keep only the exact format tag observed in captured V2 sessions.
    fn session_state_metadata(state: &serde_json::Value) -> serde_json::Value {
        let mut kept = serde_json::Map::new();
        if state.get("version").and_then(|value| value.as_str()) == Some("v1") {
            kept.insert("version".into(), serde_json::Value::String("v1".into()));
        }
        serde_json::Value::Object(kept)
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

        // Two layouts, and neither is the home directory the loop above
        // accepted: the flat one is `<cli home>/sessions/cli`, the bucketed one
        // is `<root>/sessions/<bucket>/<id>/session.json` under either root. An
        // IDE-only install has the second and not the first, which is exactly
        // the case that used to read as "installed, no sessions".
        if installed {
            if let Some(cli) = Self::sessions_dir() {
                evidence.push(store_evidence(&cli));
            }
            for root in Self::kas_roots() {
                evidence.push(store_evidence(&root));
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
        // `sessions/` rather than `sessions/cli/`, because it is the parent of
        // both layouts and callers match explicitly-passed paths against these
        // roots with `starts_with`.
        //
        // Every root, not just the IDE's: `ide_root()` alone ignores
        // `KIRO_HOME` by design, so with it set the whole CLI store — flat
        // *and* bucketed — sat under no returned root, and
        // `casr info $KIRO_HOME/sessions/cli/<id>.json` fell through to the
        // best-effort parser and was read as some other agent's format.
        Self::kas_roots()
            .into_iter()
            .filter(|d| d.is_dir())
            .collect()
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
        // The bucketed layout keys on a one-way workspace hash, so the bucket
        // holding a given id can only be found by looking in all of them —
        // which is what Kiro's own `deleteSessionAcrossBuckets` does.
        // `owns_session` answers "is this id mine"; a directory it could not
        // read makes the answer `None`, which is what "not mine" already means
        // here, so the failures are collected and dropped rather than reported
        // through a return type that has nowhere to put them.
        Self::kas_sessions(&mut Vec::new())
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

        // The bucketed layout, under every root. Same `seen` set, so the
        // session id remains the one key: a session cannot be listed twice
        // because two layouts — or two coinciding roots — describe it.
        let mut kas_unreadable = Vec::new();
        for (id, path) in Self::kas_sessions(&mut kas_unreadable) {
            push(id, path, &mut seen);
        }

        listing.sessions = sessions;
        listing.unreadable.extend(kas_unreadable);
        Some(listing)
    }

    /// Two layouts, two rules. The flat one is a `<id>.json`/`<id>.jsonl`/
    /// `<id>.history` triplet in `sessions/cli`; the bucketed one is
    /// `sessions/<bucket>/<id>/session.json`. `.history` is the raw journal of
    /// an already-listed session and `tool-outputs/` below the bucketed anchor
    /// holds spilled payloads, so neither is a session of its own.
    fn is_session_path(&self, path: &Path) -> bool {
        if Self::is_kas_anchor(path) {
            return true;
        }
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("json" | "jsonl")
        )
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        if Self::is_kas_anchor(path) {
            return read_ide_session(path);
        }
        debug!(path = %path.display(), "reading Kiro session");

        // The given `path` may be either the `.json` metadata or the `.jsonl`
        // journal; resolve both siblings regardless.
        let json_path = Self::sibling(path, "json");
        let jsonl_path = Self::sibling(path, "jsonl");

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
        let mut previous_group: Option<(String, u8)> = None;
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
                        previous_group = None;
                        continue;
                    }
                };
                let Some(mut msg) = parse_envelope(&envelope, &mut ended_at) else {
                    previous_group = None;
                    continue;
                };
                let group = generated_message_group(&envelope);
                let joins_previous = match (&previous_group, &group) {
                    (Some((previous_id, previous_rank)), Some((id, rank, _))) => {
                        previous_id == id
                            && (previous_rank < rank || (*previous_rank == 2 && *rank == 2))
                    }
                    _ => false,
                };
                if let Some((_, _, role)) = &group {
                    msg.role = role.clone();
                }

                // A canonical message can carry prose/calls and results
                // together, while Kiro consumes text, calls, and results
                // from different variants. Merge only consecutive,
                // ordered groups carrying the writer's validated id;
                // ordinary native UUIDs remain independent even if a
                // vendor happens to repeat one.
                if joins_previous && let Some(previous) = messages.last_mut() {
                    if !msg.content.is_empty() {
                        if !previous.content.is_empty() {
                            previous.content.push_str("\n\n");
                        }
                        previous.content.push_str(&msg.content);
                    }
                    if previous.timestamp.is_none() {
                        previous.timestamp = msg.timestamp;
                    }
                    if previous.author.is_none() {
                        previous.author = msg.author;
                    }
                    previous.tool_calls.append(&mut msg.tool_calls);
                    previous.tool_results.append(&mut msg.tool_results);
                    previous_group = group.map(|(id, rank, _)| (id, rank));
                    continue;
                }

                previous_group = group.map(|(id, rank, _)| (id, rank));
                messages.push(msg);
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
        //
        // Deliberately not read, and deliberately not carried.
        //
        // The module doc used to call `.history` a "slash-command history",
        // and that is what made copying it whole look harmless. It is not true.
        // Driving the shipped 2.14.2 prompt under a pty writes *every*
        // submitted line to `sessions/cli/<id>.history` — plain prompts and
        // slash commands alike. The writer is `addToHistory` in the bundled
        // `~/.local/share/kiro-cli/tui.js`:
        //
        // ```js
        // addToHistory(e){let n=e.trim();if(!n)return;
        //   if(this.history.length>0&&this.history[0]===n)return;
        //   if(this.history.unshift(n),this.history.length>100)this.history.pop()}
        // ```
        //
        // Empty lines and consecutive duplicates are suppressed; nothing else
        // is. No slash gate, no secret filter, capped at 100 entries. The
        // `--legacy-ui` path (rustyline 15.0.0, `InputSource::read_line`)
        // reaches the same conclusion by a different route, and disassembly
        // finds no `'/'` gate in either.
        //
        // So `.history` is 100 lines of raw user input, and a key pasted at
        // the prompt is in it verbatim. `casr info --json` prints the metadata
        // bag, and users pipe that to a file and paste it into issues. There
        // is no allow-list to apply — the file has no fields, only the user's
        // typing — so the only correct filter is not to carry it.
        //
        // The cost is real and accepted: `write_session` below re-emits
        // `.history` from `metadata["history"]`, so a Kiro→…→Kiro round-trip
        // no longer restores the prompt history. Recall convenience is not
        // worth republishing everything the user ever typed. `write_session`
        // still honours the key if some other reader ever supplies it.

        // --- Metadata bag (preserved for round-trip fidelity) -------------
        let mut metadata = serde_json::Map::new();
        metadata.insert("source".into(), serde_json::Value::String(SLUG.to_string()));
        if let Some(state) = meta.get("session_state") {
            metadata.insert("session_state".into(), Self::session_state_metadata(state));
        }
        for key in ["parent_session_id", "session_created_reason"] {
            if let Some(v) = meta.get(key)
                && !v.is_null()
            {
                metadata.insert(key.into(), v.clone());
            }
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
        // Both fields are optional in Kiro's loader. `session_created_reason`
        // is not nullable and accepts exactly these two enum members; emitting
        // `null` makes the entire session unreadable.
        if let Some(parent) = session
            .metadata
            .get("parent_session_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            meta.insert(
                "parent_session_id".into(),
                serde_json::Value::String(parent.to_string()),
            );
        }
        if let Some(reason @ ("subagent" | "rewind")) = session
            .metadata
            .get("session_created_reason")
            .and_then(|value| value.as_str())
        {
            meta.insert(
                "session_created_reason".into(),
                serde_json::Value::String(reason.to_string()),
            );
        }
        // Never replay vendor-owned state from canonical metadata. The shipped
        // loader accepts this minimal state; omitted runtime/request/permission
        // snapshots are not needed to load the transcript.
        meta.insert(
            "session_state".into(),
            serde_json::json!({ "version": "v1" }),
        );

        let json_bytes =
            serde_json::to_string_pretty(&serde_json::Value::Object(meta))?.into_bytes();

        // --- Conversation journal (.jsonl) --------------------------------
        let mut jsonl = String::new();
        for msg in &session.messages {
            for envelope in message_to_envelopes(msg) {
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

    /// One flag, two contracts — and the bucketed one has two extra
    /// preconditions casr used to omit.
    ///
    /// `kiro-cli --resume-id <id>` is implemented by handing the id to the
    /// internal wire subcommand `chat _ ensure-session`, verbatim from the
    /// shipped TUI bundle:
    ///
    /// ```js
    /// let Q = await md({ sourceFormat: "auto", sourceSessionId: zn.resumeId,
    ///                    targetFormat: Lc(), cwd: process.cwd() });
    /// // md: ["chat","_","ensure-session","--source-format",…,"--cwd",…]
    /// ```
    ///
    /// So the resolution is scoped by `process.cwd()` and by the agent engine,
    /// and both matter. Measured against `kiro-cli-chat` 2.14.2, on one
    /// bucketed session in `…/ws-demo`:
    ///
    /// ```text
    /// --cwd …/ws-demo --target-format kas → {"kind":"ensureSession","data":{"sessionId":"sess_9c1f…"}}
    /// --cwd …/ws-imp  --target-format kas → {"kind":"error","data":{…,"code":"SESSION_NOT_FOUND"}}
    /// --cwd …/ws-demo --target-format v2  → {"kind":"error","data":{"message":"ensure-session: KAS source -> V2 target not supported"}}
    /// ```
    ///
    /// `--target-format` follows the engine, and the engine defaults to v2
    /// (`--agent-engine <ENGINE>  … "v1", "v2" (default), or "v3"`), so a
    /// bucketed session needs `--v3` *and* the workspace as the working
    /// directory. A flat session needs neither: the same probe resolves a
    /// `sessions/cli` uuid from `/tmp` and from an unrelated workspace alike.
    ///
    /// The Kiro IDE contributes nothing here. It has no CLI-invocable resume
    /// for a session that already exists on disk: its only deep link is
    /// `kiro://kiro.resume-session/<base64 presigned-URL>`, which downloads and
    /// unpacks a *remote* zip into a folder the user picks, and its only local
    /// affordance is the palette command `kiroAgent.openChatSession`. So when a
    /// bucketed session has no workspace to `cd` into — `workspacePaths: []`,
    /// which Kiro buckets under the literal `_global` and which no `process.cwd()`
    /// can ever hash to — there is no command that resumes it, and casr says so
    /// by naming no session at all rather than printing one that cannot work.
    fn resume_command(&self, session_id: &str) -> String {
        match Self::resume_form(session_id) {
            ResumeForm::Flat => format!("kiro-cli --resume-id {session_id}"),
            ResumeForm::Bucketed(workspace) => {
                // Quoted, because a workspace path with a space in it is
                // ordinary and `cd /Users/a b` is not a `cd` into `/Users/a b`.
                let dir = workspace.display().to_string();
                let cd = shlex::try_join(["cd", &dir]).unwrap_or_else(|_| format!("cd {dir}"));
                format!("{cd} && kiro-cli --v3 --resume-id {session_id}")
            }
            ResumeForm::Unreachable => "kiro".to_string(),
        }
    }

    /// Built directly rather than recovered from [`Self::resume_command`].
    ///
    /// The trait default splits the rendered string into a program and
    /// arguments, which for the bucketed form would launch `cd`. The working
    /// directory is a first-class field of a [`LaunchSpec`] precisely because
    /// it is load-bearing here, so it is set rather than rendered.
    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        Some(match Self::resume_form(session_id) {
            ResumeForm::Flat => LaunchSpec::new(
                "kiro-cli",
                ["--resume-id".to_string(), session_id.to_string()],
            )
            .targeting_session(session_id),
            ResumeForm::Bucketed(workspace) => LaunchSpec::new(
                "kiro-cli",
                [
                    "--v3".to_string(),
                    "--resume-id".to_string(),
                    session_id.to_string(),
                ],
            )
            .in_dir(workspace)
            .targeting_session(session_id),
            // Deliberately not `targeting_session`: `kiro` opens the IDE and
            // names nothing, which is what the caller has to be told.
            ResumeForm::Unreachable => LaunchSpec::new("kiro", Vec::new()),
        })
    }
}

/// How — or whether — a given session id can be resumed from a shell.
///
/// See [`Kiro::resume_command`] for the measurements behind each arm.
enum ResumeForm {
    /// A flat `sessions/cli/<uuid>` session: `--resume-id` finds it from
    /// anywhere. Also the form every session casr *writes* takes.
    Flat,
    /// A bucketed session with a workspace: reachable only from that directory.
    Bucketed(PathBuf),
    /// A bucketed session bucketed under `_global`: no shell command resumes it.
    Unreachable,
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
        // Not a lifecycle marker: `session_start.content` is the prompt that
        // opened the session, and it is the *only* copy of it — Kiro writes it
        // once, on the first turn, and writes no `user` payload for that turn.
        // Its own model-context rebuild replays it as a human message before
        // anything else. Dropping it dropped the first thing the user said.
        // See the module docs for the two shipped functions.
        "session_start" => (
            MessageRole::User,
            text("content"),
            Some("user".to_string()),
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
        // No Kiro writer consumes the native event, and the canonical fields
        // above contain everything this reader can safely carry.
        extra: serde_json::Value::Null,
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
        // The writer rebuilds a bounded envelope from the typed fields above.
        // Keeping the native envelope here would retain the same unbounded
        // vendor blob after refusing to publish it.
        extra: serde_json::Value::Null,
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

/// Recognize one envelope in a group synthesized by [`message_to_envelopes`].
///
/// Native Kiro ids are UUIDs. Requiring the private prefix, a valid role code,
/// a valid UUID, and a legal variant keeps duplicate native ids from becoming
/// an accidental instruction to coalesce unrelated turns.
fn generated_message_group(envelope: &serde_json::Value) -> Option<(String, u8, MessageRole)> {
    let id = envelope.pointer("/data/message_id")?.as_str()?;
    let mut parts = id.split(':');
    if parts.next() != Some("agsx") {
        return None;
    }
    let role_code = parts.next()?;
    let uuid = parts.next()?;
    if parts.next().is_some() || uuid::Uuid::parse_str(uuid).is_err() {
        return None;
    }

    let kind = envelope.get("kind")?.as_str()?;
    let (rank, role) = match (role_code, kind) {
        ("u", "Prompt") => (0, MessageRole::User),
        ("u", "AssistantMessage") => (1, MessageRole::User),
        ("u", "ToolResults") => (2, MessageRole::User),
        ("a", "AssistantMessage") => (1, MessageRole::Assistant),
        ("a", "ToolResults") => (2, MessageRole::Assistant),
        ("t", "AssistantMessage") => (1, MessageRole::Tool),
        ("t", "ToolResults") => (2, MessageRole::Tool),
        _ => return None,
    };
    Some((id.to_string(), rank, role))
}

/// Serialize a canonical message into up to three Kiro `.jsonl` envelopes.
///
/// `CanonicalMessage::extra` is untrusted vendor data. Always synthesize a
/// bounded envelope from the canonical fields, regardless of origin.
fn message_to_envelopes(msg: &CanonicalMessage) -> Vec<serde_json::Value> {
    // `kind` is not a free-form label. The journal line is an adjacently tagged
    // Rust enum, and the shipped `kiro-cli-chat` 2.14.2 binary names it and its
    // whole membership in the serde strings it carries — `adjacently tagged
    // enum LogEntryV1`, then `struct variant LogEntryV1::Prompt`,
    // `::AssistantMessage`, `::ToolResults`, `::Compaction`, `::ResetTo`, and
    // the field list they are built from, `V1 Prompt message_id content
    // ToolResults results Compaction summary messages_snapshot ResetTo
    // target_index`. Five variants, all of them struct variants, so there is no
    // `#[serde(other)]` unit arm to absorb a sixth: a `kind` outside that set is
    // an `unknown variant` deserialization error, not a graceful degradation.
    // Inventing a `System` kind is therefore not one of the options.
    //
    // Of the five, `Compaction` (a summary plus a snapshot) and `ResetTo` (an
    // index) hold no turn. That leaves three, and a system prompt or an
    // unrecognised source role has to become one of them. `Prompt` — the same
    // place `System` already goes — anonymises the operator, which
    // `pipeline::folded_role` declares as a `Loss`. `AssistantMessage` does not
    // anonymise anything; it tells the resumed agent that it issued the
    // instruction itself. The first is recoverable news, the second is a
    // falsehood about who holds authority in the session.
    let role_code = match msg.role {
        MessageRole::User | MessageRole::System | MessageRole::Other(_) => "u",
        MessageRole::Assistant => "a",
        MessageRole::Tool => "t",
    };

    let content_is_structural_result = matches!(msg.role, MessageRole::Tool)
        && !msg.content.trim().is_empty()
        && msg
            .tool_results
            .iter()
            .any(|result| result.content == msg.content);
    let mut text_content: Vec<serde_json::Value> = Vec::new();
    if !msg.content.is_empty() && !content_is_structural_result {
        text_content.push(serde_json::json!({ "kind": "text", "data": msg.content }));
    }
    let mut call_content: Vec<serde_json::Value> = Vec::new();
    for tc in &msg.tool_calls {
        call_content.push(serde_json::json!({
            "kind": "toolUse",
            "data": {
                "toolUseId": tc.id.clone().unwrap_or_default(),
                "name": tc.name,
                "input": tc.arguments,
            }
        }));
    }

    let mut result_payloads = Vec::with_capacity(msg.tool_results.len());
    for (index, tr) in msg.tool_results.iter().enumerate() {
        let call_id = tr.call_id.clone().unwrap_or_default();
        let mut content = Vec::new();
        if content_is_structural_result && index == 0 {
            // Kiro ignores ToolResults.content, but casr uses this bounded copy
            // to restore readers that expose the same observation as both text
            // and a structural result without showing it twice to the model.
            content.push(serde_json::json!({ "kind": "text", "data": msg.content }));
        }
        content.push(serde_json::json!({
            "kind": "toolResult",
            "data": {
                "toolUseId": call_id.clone(),
                "content": [{ "kind": "text", "data": tr.content }],
                "status": if tr.is_error { "error" } else { "success" },
            }
        }));
        let result = if tr.is_error {
            serde_json::json!({ "Error": { "Custom": tr.content } })
        } else {
            serde_json::json!({ "Success": { "items": [{ "Text": tr.content }] } })
        };
        let mut results = serde_json::Map::new();
        results.insert(
            call_id,
            serde_json::json!({
                "tool": {
                    "kind": {
                        "BuiltIn": {
                            "ExecuteCmd": { "command": "" }
                        }
                    }
                },
                "result": result,
            }),
        );
        // One result per map preserves duplicate and missing call ids. Kiro
        // accepts repeated ToolResults records in one correlated group.
        result_payloads.push((content, results));
    }

    let message_id = format!("agsx:{role_code}:{}", uuid::Uuid::new_v4());
    let envelope =
        |kind: &str,
         content: Vec<serde_json::Value>,
         results: Option<serde_json::Map<String, serde_json::Value>>| {
            let mut data = serde_json::Map::new();
            data.insert(
                "message_id".into(),
                serde_json::Value::String(message_id.clone()),
            );
            data.insert("content".into(), serde_json::Value::Array(content));
            if kind == "Prompt"
                && let Some(ts) = msg.timestamp
            {
                data.insert("meta".into(), serde_json::json!({ "timestamp": ts / 1000 }));
            }
            if let Some(results) = results {
                data.insert("results".into(), serde_json::Value::Object(results));
            }
            serde_json::json!({
                "version": "v1",
                "kind": kind,
                "data": serde_json::Value::Object(data),
            })
        };

    // Kiro 2.15 consumes Prompt text, AssistantMessage text/toolUse, and
    // ToolResults.results. `data.content` on ToolResults is accepted but never
    // replayed. Split by the vendor's real channels, then let the reader join
    // the bounded group back into the original canonical message.
    let mut envelopes = Vec::with_capacity(2 + result_payloads.len());
    match msg.role {
        MessageRole::User | MessageRole::System | MessageRole::Other(_) => {
            if !text_content.is_empty() {
                envelopes.push(envelope("Prompt", text_content, None));
            }
            if !call_content.is_empty() {
                envelopes.push(envelope("AssistantMessage", call_content, None));
            }
        }
        MessageRole::Assistant | MessageRole::Tool => {
            text_content.extend(call_content);
            if !text_content.is_empty() {
                envelopes.push(envelope("AssistantMessage", text_content, None));
            }
        }
    }

    for (content, results) in result_payloads {
        envelopes.push(envelope("ToolResults", content, Some(results)));
    }
    envelopes
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

        // The `.history` sidecar is deliberately *not* captured. It is not the
        // slash-command log this reader once took it for: kiro-cli's
        // `addToHistory` appends every submitted line, so the file is raw user
        // input and `casr info --json` would print it. See the comment at the
        // read site. The fixture still has one, so this asserts the drop and
        // not merely its absence.
        assert!(
            fixture_json_path().with_extension("history").is_file(),
            "the fixture must keep a .history sidecar, or this asserts nothing"
        );
        assert!(
            session.metadata.get("history").is_none(),
            "`.history` is 100 lines of whatever the user typed at the prompt; \
             it must not reach the metadata bag that `info --json` prints"
        );
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

        // Re-emit every message, including multi-result turns that require
        // several ToolResults envelopes, then exercise the real reader's
        // bounded group reassembly.
        let mut journal = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        for message in &original.messages {
            for envelope in message_to_envelopes(message) {
                writeln!(journal, "{}", serde_json::to_string(&envelope).unwrap()).unwrap();
            }
        }
        journal.flush().unwrap();
        let reparsed = Kiro.read_session(journal.path()).unwrap().messages;

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

        let u_envs = message_to_envelopes(&user);
        assert_eq!(u_envs.len(), 1);
        let u_env = &u_envs[0];
        assert_eq!(u_env["kind"], "Prompt");
        // Prompt timestamps are emitted as epoch seconds under data.meta.
        assert_eq!(u_env["data"]["meta"]["timestamp"], 1_700_000_000);

        let a_envs = message_to_envelopes(&assistant);
        assert_eq!(a_envs.len(), 2);
        let a_env = &a_envs[0];
        let result_env = &a_envs[1];
        assert_eq!(a_env["kind"], "AssistantMessage");
        assert_eq!(result_env["kind"], "ToolResults");
        assert!(
            a_env["data"]["message_id"]
                .as_str()
                .unwrap()
                .starts_with("agsx:a:")
        );
        assert_eq!(
            a_env["data"]["message_id"], result_env["data"]["message_id"],
            "split records must rejoin during read-back"
        );
        assert_eq!(
            result_env["data"]["results"]["t1"]["result"]["Success"]["items"][0]["Text"],
            "file.txt"
        );

        let mut ended = None;
        let ru = parse_envelope(u_env, &mut ended).unwrap();
        let ra = parse_envelope(a_env, &mut ended).unwrap();
        let rr = parse_envelope(result_env, &mut ended).unwrap();
        assert_eq!(ru.content, "Hi there");
        assert_eq!(ra.content, "Hello back");
        assert_eq!(ra.tool_calls.len(), 1);
        assert_eq!(ra.tool_calls[0].name, "shell");
        assert_eq!(rr.tool_results.len(), 1);
        assert_eq!(rr.tool_results[0].content, "file.txt");
    }

    #[test]
    fn native_or_malformed_group_ids_do_not_merge_messages() {
        for id in ["00000000-0000-4000-8000-000000000001", "agsx:a:not-a-uuid"] {
            let mut journal = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
            writeln!(
                journal,
                "{}",
                serde_json::json!({
                    "version": "v1",
                    "kind": "AssistantMessage",
                    "data": {
                        "message_id": id,
                        "content": [{"kind": "text", "data": "first"}]
                    }
                })
            )
            .unwrap();
            writeln!(
                journal,
                "{}",
                serde_json::json!({
                    "version": "v1",
                    "kind": "ToolResults",
                    "data": {
                        "message_id": id,
                        "content": [{
                            "kind": "toolResult",
                            "data": {
                                "toolUseId": "call-1",
                                "content": [{"kind": "text", "data": "second"}],
                                "status": "success"
                            }
                        }]
                    }
                })
            )
            .unwrap();
            journal.flush().unwrap();

            let messages = Kiro.read_session(journal.path()).unwrap().messages;
            assert_eq!(messages.len(), 2, "id {id} must not authorize merging");
            assert_eq!(messages[0].role, MessageRole::Assistant);
            assert_eq!(messages[1].role, MessageRole::Tool);
        }
    }

    #[test]
    fn generated_groups_do_not_merge_across_non_message_records() {
        let id = "agsx:a:00000000-0000-4000-8000-000000000001";
        for separator in [
            "this is not json at all",
            r#"{"version":"v1","kind":"Compaction","data":{"summary":"older context","messages_snapshot":[]}}"#,
        ] {
            let mut journal = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
            writeln!(
                journal,
                "{}",
                serde_json::json!({
                    "version": "v1",
                    "kind": "AssistantMessage",
                    "data": {
                        "message_id": id,
                        "content": [{"kind": "text", "data": "first"}]
                    }
                })
            )
            .unwrap();
            writeln!(journal, "{separator}").unwrap();
            writeln!(
                journal,
                "{}",
                serde_json::json!({
                    "version": "v1",
                    "kind": "ToolResults",
                    "data": {
                        "message_id": id,
                        "content": [{
                            "kind": "toolResult",
                            "data": {
                                "toolUseId": "call-1",
                                "content": [{"kind": "text", "data": "second"}],
                                "status": "success"
                            }
                        }]
                    }
                })
            )
            .unwrap();
            journal.flush().unwrap();

            let messages = Kiro.read_session(journal.path()).unwrap().messages;
            assert_eq!(
                messages.len(),
                2,
                "separator {separator:?} must break a generated group"
            );
            assert_eq!(messages[0].content, "first");
            assert_eq!(messages[1].tool_results[0].content, "second");
        }
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
