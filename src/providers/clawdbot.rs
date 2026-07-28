//! ClawdBot provider — reads/writes `pi-coding-agent` `SessionManager` JSONL.
//!
//! Session files, both of which occur on a live machine:
//! - `~/.clawdbot/agents/<agent-id>/sessions/*.jsonl` — current
//! - `~/.clawdbot/sessions/*.jsonl` — through `clawdbot@2026.1.2x`
//!
//! Override root: `CLAWDBOT_HOME` (casr's own), else `CLAWDBOT_STATE_DIR`.
//!
//! ## JSONL format
//!
//! ClawdBot does not have a session format of its own. It depends on
//! `@mariozechner/pi-coding-agent` and drives the transcript through that
//! package's `SessionManager` — `dist/agents/pi-embedded-runner.js` calls
//! `SessionManager.open(params.sessionFile)`, and
//! `dist/config/sessions/transcript.js` writes the header and appends through
//! `sessionManager.appendMessage(...)`. Every published version of the package,
//! from `clawdbot@2026.1.4` to `clawdbot@2026.1.24-3`, writes that envelope:
//!
//! ```json
//! {"type":"session","version":2,"id":"…","timestamp":"…","cwd":"/home/u/p"}
//! {"type":"message","id":"5c4f098c","parentId":null,"timestamp":"…","message":{"role":"user","content":"…"}}
//! ```
//!
//! The parse lives in [`crate::providers::pi_session`], shared with the `pi`
//! provider, which reads the output of the very same library. That module's
//! docs list every record type the envelope can carry and say why OpenClaw —
//! which forked away from `pi-coding-agent` — is deliberately not folded in.
//!
//! ## Storage layout, and why both are read
//!
//! `clawdbot@2026.1.4` stored transcripts at `~/.clawdbot/sessions/<id>.jsonl`
//! (`dist/config/sessions.js`: `resolveSessionTranscriptsDir`). By
//! `clawdbot@2026.1.24-3` they had moved under the agent
//! (`dist/config/sessions/paths.js`: `<state>/agents/<agentId>/sessions`), and
//! `dist/infra/state-migrations.js` moves the old files across on startup.
//!
//! That migration is not a guarantee that only one layout exists. It runs when
//! the gateway starts, it skips any file whose name already exists in the
//! target, and when a file is left behind it renames the whole directory to
//! `sessions.legacy-<timestamp>`. A machine that has not been upgraded, or has
//! not started the gateway since upgrading, has only the old layout. So both
//! are read, and neither can be assumed away.
//!
//! ## Session ID scheme
//!
//! Sessions are identified by the filename stem, which is the id ClawdBot
//! allocated: `resolveSessionTranscriptPath` names the file `<sessionId>.jsonl`
//! and `dist/agents/pi-embedded-runner/session-manager-init.js` stamps the same
//! id into the header. The stem is preferred over the header id because the
//! stem is what `owns_session` resolved and what a resume command needs.

use std::path::{Path, PathBuf};

use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::model::{CanonicalSession, MessageRole, truncate_title};
use crate::providers::pi_session;
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession,
    filename_safe_session_id, read_dir_reporting, walk_entry_reporting,
};

/// ClawdBot's default agent id (`dist/routing/session-key.js`:
/// `DEFAULT_AGENT_ID = "main"`). Sessions are keyed by agent and the id is
/// mandatory in the path, so casr writes as the agent ClawdBot itself defaults
/// to rather than inventing one.
const DEFAULT_AGENT_ID: &str = "main";

/// ClawdBot provider implementation.
pub struct ClawdBot;

impl ClawdBot {
    /// ClawdBot's mutable state root — `CLAWDBOT_STATE_DIR` if set, else
    /// `~/.clawdbot`, exactly as `dist/config/paths.js: resolveStateDir` does
    /// it. An empty value counts as unset, matching ClawdBot, which trims the
    /// override and ignores it when blank.
    fn state_dir() -> PathBuf {
        if let Some(state) =
            std::env::var_os("CLAWDBOT_STATE_DIR").filter(|value| !value.is_empty())
        {
            return PathBuf::from(state);
        }
        dirs::home_dir().unwrap_or_default().join(".clawdbot")
    }

    /// Where casr writes, in precedence order:
    ///
    /// 1. `CLAWDBOT_HOME` — casr's own override, naming the sessions directory
    ///    itself. ClawdBot has no variable with those semantics, so this one is
    ///    casr's alone; it wins so that aiming casr at a tree never disturbs the
    ///    ClawdBot the rest of the shell talks to.
    /// 2. `<state>/agents/main/sessions` — where current ClawdBot looks.
    fn home_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("CLAWDBOT_HOME").filter(|value| !value.is_empty()) {
            return PathBuf::from(home);
        }
        Self::state_dir()
            .join("agents")
            .join(DEFAULT_AGENT_ID)
            .join("sessions")
    }

    /// Every directory that can hold a ClawdBot transcript, current layout
    /// first. Only directories that exist are returned.
    ///
    /// `CLAWDBOT_HOME` names the sessions directory outright, so when it is set
    /// it is the whole answer — the point of that override is that casr looks
    /// nowhere else.
    fn session_dirs_reporting(unreadable: &mut Vec<UnreadableSource>) -> Vec<PathBuf> {
        if let Some(home) = std::env::var_os("CLAWDBOT_HOME").filter(|value| !value.is_empty()) {
            let home = PathBuf::from(home);
            return if home.is_dir() { vec![home] } else { vec![] };
        }

        let state = Self::state_dir();
        let mut dirs: Vec<PathBuf> = Vec::new();

        // Current: one sessions directory per agent. An `agents/` that exists
        // and cannot be read is reported rather than yielding zero agents: it
        // hides every session on the machine, and the caller that swallowed it
        // reported "ClawdBot has no sessions".
        let mut agent_dirs: Vec<PathBuf> = read_dir_reporting(&state.join("agents"), unreadable)
            .into_iter()
            .map(|entry| entry.path().join("sessions"))
            .filter(|path| path.is_dir())
            .collect();
        agent_dirs.sort();
        dirs.append(&mut agent_dirs);

        // Legacy: the pre-migration flat directory.
        let legacy = state.join("sessions");
        if legacy.is_dir() {
            dirs.push(legacy);
        }

        dirs
    }

    /// `session_dirs_reporting` for the callers with nowhere to put a read
    /// failure — `detect`, `session_roots`, `owns_session`, the writer.
    fn session_dirs() -> Vec<PathBuf> {
        Self::session_dirs_reporting(&mut Vec::new())
    }
}

impl Provider for ClawdBot {
    fn name(&self) -> &str {
        "ClawdBot"
    }

    fn slug(&self) -> &str {
        "clawdbot"
    }

    fn cli_alias(&self) -> &str {
        "cwb"
    }

    fn detect(&self) -> DetectionResult {
        let dirs = Self::session_dirs();
        let state = Self::state_dir();
        // The state directory existing is enough to call ClawdBot installed:
        // the sessions directory is only created once a session runs.
        let state_exists = state.is_dir();
        let installed = !dirs.is_empty() || state_exists;
        let evidence = if !dirs.is_empty() {
            dirs.iter()
                .map(|dir| format!("sessions directory found: {}", dir.display()))
                .collect()
        } else if state_exists {
            vec![format!(
                "state directory found (no sessions yet): {}",
                state.display()
            )]
        } else {
            vec![]
        };
        trace!(provider = "clawdbot", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        Self::session_dirs()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let mut listing = SessionListing::default();
        for root in Self::session_dirs_reporting(&mut listing.unreadable) {
            // One level, because that is how far ClawdBot looks.
            // `listSessionFilesForAgent` (`dist/memory/session-files.js`) is a
            // single `fs.readdir(dir, { withFileTypes: true })` with no
            // `recursive`, filtered by `entry.isFile()`. Nothing in the shipped
            // package creates a subdirectory under an agent's `sessions/`
            // either: the only `mkdir`s that reach it — `mkdir(sessionsDir)` in
            // `session-write-lock.js` and `mkdir(path.dirname(sessionFile))` in
            // `config/sessions/transcript.js` — *are* that directory.
            //
            // The fix belongs here and not in `is_session_path`, which already
            // transcribes ClawdBot's file-name rule exactly. `max_depth(4)` was
            // walking into anything a user had put beside the transcripts —
            // `sessions/attachments/`, `sessions/archive/`, `sessions/a/b/c/` —
            // and every `.jsonl` under it passed a predicate that was never
            // asked where the file was.
            for entry in walkdir::WalkDir::new(&root).max_depth(1) {
                let Some(entry) = walk_entry_reporting(entry, &mut listing.unreadable) else {
                    continue;
                };
                let path = entry.path();
                if !entry.file_type().is_file() || !self.is_session_path(path) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                listing
                    .sessions
                    .push((stem.to_string(), path.to_path_buf()));
            }
        }
        Some(listing)
    }

    /// ClawdBot's own rule, from `dist/memory/session-files.js` in
    /// `clawdbot@2026.1.24-3`: `entry.isFile() && name.endsWith(".jsonl")`.
    ///
    /// It has to be a rule and not a glob because the directory is shared. The
    /// session store writes `sessions.json` (`dist/config/sessions/paths.js`),
    /// `sessions.json.lock` and `sessions.json.<pid>.<uuid>.tmp`
    /// (`dist/config/sessions/store.js`) into it, and a `<sessionId>.jsonl.lock`
    /// beside each transcript (`dist/agents/session-write-lock.js`). `list` was
    /// rendering `sessions.json` as a session with zero messages, because a
    /// `.json` file in a session directory was all it asked for.
    ///
    /// `.jsonl` alone is sufficient *for this tool*: the lock and temp files
    /// end in `.lock` and `.tmp`, and no other `.jsonl` is ever written there.
    /// It admits both transcript shapes, `<sessionId>.jsonl` and the
    /// `<sessionId>-topic-<topicId>.jsonl` a topic session produces.
    fn is_session_path(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        for root in Self::session_dirs() {
            let candidate = root.join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                debug!(
                    provider = "clawdbot",
                    path = %candidate.display(),
                    session_id,
                    "owns session"
                );
                return Some(candidate);
            }
            // Walk subdirectories.
            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == session_id)
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "jsonl")
                {
                    debug!(
                        provider = "clawdbot",
                        path = %entry.path().display(),
                        session_id,
                        "owns session (subdirectory)"
                    );
                    return Some(entry.path().to_path_buf());
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading ClawdBot session");

        let transcript = pi_session::read(path, "clawdbot")?;

        // The filename stem is the id ClawdBot allocated and the id a resume
        // command needs. The header id agrees with it on any session ClawdBot
        // started itself; where they disagree — a transcript copied out of
        // another agent's directory, say — the stem is the one that resolves.
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let title = transcript
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let workspace = transcript.cwd.as_ref().map(PathBuf::from);

        // `unrepresented` is present only when something was actually left
        // over. Absent means every line was accounted for; it never means "this
        // reader did not look".
        let metadata = serde_json::json!({
            "source": "clawdbot",
            "cwd": transcript.cwd,
            "header_session_id": transcript.header_id,
            "unrepresented": transcript.describe_unrepresented(),
        });

        info!(
            session_id,
            messages = transcript.messages.len(),
            unrepresented = transcript.describe_unrepresented(),
            "ClawdBot session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "clawdbot".to_string(),
            workspace,
            title,
            started_at: transcript.started_at,
            ended_at: transcript.ended_at,
            messages: transcript.messages,
            metadata,
            source_path: path.to_path_buf(),
            model_name: transcript.model_id,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let session_id = if session.session_id.is_empty() {
            format!("casr-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"))
        } else {
            session.session_id.clone()
        };
        let session_id = filename_safe_session_id(&session_id);

        let target_dir = Self::home_dir();
        let target_path = target_dir.join(format!("{session_id}.jsonl"));

        debug!(
            session_id,
            path = %target_path.display(),
            messages = session.messages.len(),
            "writing ClawdBot session"
        );

        let (content, warnings) = Self::render(&session_id, session)?;

        let outcome = crate::pipeline::atomic_write(
            &target_path,
            content.as_bytes(),
            opts.force,
            self.slug(),
        )?;

        info!(
            session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "ClawdBot session written"
        );

        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id: session_id.clone(),
            resume_command: self.resume_command(&session_id),
            backups: outcome.displaced().into_iter().collect(),
            warnings,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("clawdbot --resume {session_id}")
    }
}

impl ClawdBot {
    /// Render a session as a `SessionManager` transcript: the file bytes, plus
    /// whatever the caller has to be told was left out of them.
    ///
    /// Separate from [`Provider::write_session`] because that resolves its
    /// target from the environment, and the environment cannot be set safely
    /// from a Rust 2024 test. What gets written is worth a test of its own.
    fn render(
        session_id: &str,
        session: &CanonicalSession,
    ) -> anyhow::Result<(String, Vec<String>)> {
        let mut lines: Vec<String> = Vec::with_capacity(session.messages.len() + 1);
        let mut warnings: Vec<String> = Vec::new();

        // Session header, as ClawdBot writes it in
        // `dist/config/sessions/transcript.js: ensureSessionHeader`. `version`
        // is `CURRENT_SESSION_VERSION` = 2; writing 2 with real `id`/`parentId`
        // links means `SessionManager` loads the file as-is instead of running
        // `migrateSessionEntries` and rewriting it under the user.
        let header = serde_json::json!({
            "type": "session",
            "version": 2,
            "id": session_id,
            "timestamp": session.started_at
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            "cwd": session.workspace.as_ref().and_then(|w| w.to_str()).unwrap_or("/tmp"),
        });
        lines.push(serde_json::to_string(&header)?);

        // The entry tree. Every entry is a child of the one before it, which is
        // the shape `SessionManager.appendMessage` produces for a conversation
        // that was never branched. Ids are 8 hex characters, as
        // `generateId()` — `randomUUID().slice(0, 8)` — produces.
        let mut parent: Option<String> = None;
        let mut dropped_empty = 0usize;

        for msg in &session.messages {
            // The reader skips a message that flattens to nothing, so writing
            // one would mean writing a line that cannot survive a read-back.
            if msg.content.trim().is_empty() {
                dropped_empty += 1;
                continue;
            }

            let epoch_ms = msg
                .timestamp
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
            let ts_iso = chrono::DateTime::from_timestamp_millis(epoch_ms)
                .unwrap_or_else(chrono::Utc::now)
                .to_rfc3339();

            // One text block, never a `toolCall` block. The reader flattens
            // `toolCall` blocks into the text as `[Tool: name]`, so emitting
            // both would duplicate them on read-back.
            let text_blocks = serde_json::json!([{ "type": "text", "text": msg.content }]);

            let inner = match &msg.role {
                // `content` may be a plain string for a user message, and that
                // is what ClawdBot itself writes for one.
                MessageRole::User => serde_json::json!({
                    "role": "user",
                    "content": msg.content,
                    "timestamp": epoch_ms,
                }),
                MessageRole::Assistant => {
                    let extra = msg.extra.get("message");
                    let field = |name: &str| {
                        extra
                            .and_then(|m| m.get(name))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    };
                    serde_json::json!({
                        "role": "assistant",
                        "content": text_blocks,
                        // `api`/`provider`/`model` are required by pi's
                        // `AssistantMessage`. Carried through when the source
                        // session had them; otherwise ClawdBot's own convention
                        // for a message it synthesized rather than received —
                        // `dist/config/sessions/transcript.js` writes
                        // `openai-responses` with a non-model `model` value.
                        "api": field("api").unwrap_or_else(|| "openai-responses".to_string()),
                        "provider": field("provider").unwrap_or_else(|| "casr".to_string()),
                        "model": msg.author.clone()
                            .or_else(|| session.model_name.clone())
                            .unwrap_or_else(|| "unknown".to_string()),
                        "usage": {
                            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                            "totalTokens": 0,
                            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                        },
                        "stopReason": "stop",
                        "timestamp": epoch_ms,
                    })
                }
                // pi calls a tool's output `toolResult`, and the reader maps
                // that name back to `Tool`.
                MessageRole::Tool => serde_json::json!({
                    "role": "toolResult",
                    "toolCallId": msg.tool_results.first()
                        .and_then(|r| r.call_id.clone())
                        .unwrap_or_default(),
                    "toolName": "unknown",
                    "content": text_blocks,
                    "isError": msg.tool_results.first().is_some_and(|r| r.is_error),
                    "timestamp": epoch_ms,
                }),
                // pi has no system role and no role of its own for anything
                // else. The name is written through unchanged rather than
                // remapped: a role this writer cannot express is better left
                // legible than relabelled as something it is not.
                MessageRole::System => serde_json::json!({
                    "role": "system",
                    "content": msg.content,
                    "timestamp": epoch_ms,
                }),
                MessageRole::Other(role) => serde_json::json!({
                    "role": role,
                    "content": msg.content,
                    "timestamp": epoch_ms,
                }),
            };

            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            let entry = serde_json::json!({
                "type": "message",
                "id": id,
                "parentId": parent,
                "timestamp": ts_iso,
                "message": inner,
            });
            parent = Some(id);
            lines.push(serde_json::to_string(&entry)?);
        }

        if dropped_empty > 0 {
            warnings.push(format!(
                "{dropped_empty} message(s) had no text content and were not written: \
                 ClawdBot's reader skips an entry that flattens to nothing"
            ));
        }

        Ok((lines.join("\n") + "\n", warnings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CanonicalMessage;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn read_clawdbot(lines: &[&str]) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(tmp.path(), "test.jsonl", lines);
        ClawdBot.read_session(&path).expect("read_session failed")
    }

    /// The real fixture: a transcript written by the published
    /// `@mariozechner/pi-coding-agent` `SessionManager`.
    fn real_fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/clawdbot/clawdbot_simple.jsonl")
    }

    // -----------------------------------------------------------------------
    // Reader — the format ClawdBot actually writes
    // -----------------------------------------------------------------------

    /// The regression that started this: every published ClawdBot writes the
    /// `SessionManager` envelope, and a reader that looks for top-level `role`
    /// and `content` finds neither on any line, so it returns `Ok` with an
    /// empty session — indistinguishable, to whoever ran `casr list`, from
    /// having no sessions at all.
    #[test]
    fn reads_a_real_session_manager_transcript() {
        let session = ClawdBot
            .read_session(&real_fixture_path())
            .expect("the real fixture must parse");

        assert_eq!(
            session.messages.len(),
            6,
            "a transcript in the real envelope must not read as an empty session"
        );
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(
            session.messages[0].content,
            "check the gateway logs and tell me why whatsapp keeps reconnecting"
        );
        assert_eq!(session.messages[2].role, MessageRole::Tool);
        assert_eq!(
            session.title.as_deref(),
            Some("check the gateway logs and tell me why whatsapp keeps reconnecting")
        );
        assert_eq!(
            session.workspace.as_deref(),
            Some(Path::new("/home/mario/projects/clawdbot")),
            "the session header carries cwd; the old reader reported no workspace"
        );
        assert_eq!(session.model_name.as_deref(), Some("gpt-5-codex"));
        assert!(session.started_at.is_some());
        assert!(session.ended_at.is_some());
    }

    #[test]
    fn reader_basic_exchange() {
        let session = read_clawdbot(&[
            r#"{"type":"session","version":2,"id":"s1","timestamp":"2026-02-14T09:12:03.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"Hello there","timestamp":1771060324000}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:09.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi!"}]}}"#,
        ]);

        assert_eq!(session.provider_slug, "clawdbot");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello there");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Hi!");
        assert!(session.started_at.is_some());
        assert!(session.ended_at.is_some());
    }

    #[test]
    fn reader_extracts_tool_calls_and_thinking() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"read the log"},{"type":"text","text":"Pulling it."},{"type":"toolCall","id":"c1","name":"bash","arguments":{"cmd":"tail"}}]}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].content,
            "[Thinking] read the log\nPulling it.\n[Tool: bash]"
        );
        assert_eq!(session.messages[0].tool_calls.len(), 1);
        assert_eq!(session.messages[0].tool_calls[0].name, "bash");
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"assistant","content":"Welcome"}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:05.000Z","message":{"role":"user","content":"Refactor the authentication module"}}"#,
        ]);
        assert_eq!(
            session.title.as_deref(),
            Some("Refactor the authentication module")
        );
    }

    #[test]
    fn reader_skips_empty_content() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"Hello"}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:05.000Z","message":{"role":"assistant","content":""}}"#,
            r#"{"type":"message","id":"c3","parentId":"b2","timestamp":"2026-02-14T09:12:06.000Z","message":{"role":"assistant","content":"  "}}"#,
            r#"{"type":"message","id":"d4","parentId":"c3","timestamp":"2026-02-14T09:12:07.000Z","message":{"role":"assistant","content":"Real response"}}"#,
        ]);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "Hello");
        assert_eq!(session.messages[1].content, "Real response");
    }

    #[test]
    fn reader_skips_invalid_json() {
        let session = read_clawdbot(&[
            "",
            "not-json",
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"Valid line"}}"#,
            "{truncated...",
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Valid line");
    }

    #[test]
    fn reader_system_role() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"system","content":"You are helpful."}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:05.000Z","message":{"role":"user","content":"Hi"}}"#,
        ]);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[1].role, MessageRole::User);
    }

    #[test]
    fn reader_session_id_from_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "my-session.jsonl",
            &[
                r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"test"}}"#,
            ],
        );
        let session = ClawdBot.read_session(&path).unwrap();
        assert_eq!(session.session_id, "my-session");
    }

    /// The filename stem is the id ClawdBot allocated and the id a resume
    /// command needs; a header id copied in from elsewhere must not displace it.
    #[test]
    fn reader_prefers_the_filename_stem_over_a_disagreeing_header_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "on-disk-id.jsonl",
            &[
                r#"{"type":"session","version":2,"id":"header-id","timestamp":"2026-02-14T09:12:03.000Z","cwd":"/w"}"#,
            ],
        );
        let session = ClawdBot.read_session(&path).unwrap();
        assert_eq!(session.session_id, "on-disk-id");
        assert_eq!(session.metadata["header_session_id"], "header-id");
    }

    #[test]
    fn reader_empty_file() {
        let session = read_clawdbot(&[]);
        assert_eq!(session.messages.len(), 0);
        assert!(session.title.is_none());
    }

    #[test]
    fn reader_timestamps_parsed() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"First"}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T10:00:00.000Z","message":{"role":"assistant","content":"Second"}}"#,
        ]);
        assert!(session.started_at.unwrap() < session.ended_at.unwrap());
        assert!(session.messages[0].timestamp.is_some());
        assert!(session.messages[1].timestamp.is_some());
    }

    #[test]
    fn reader_metadata_has_source() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"test"}}"#,
        ]);
        assert_eq!(session.metadata["source"], "clawdbot");
    }

    /// A record type the flat track cannot hold is reported, not skipped: the
    /// whole point of the field is that `casr info --json` can say what was in
    /// the file and did not survive.
    #[test]
    fn reader_reports_records_it_cannot_represent() {
        let session = read_clawdbot(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"compaction","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:20:00.000Z","summary":"…","firstKeptEntryId":"a1","tokensBefore":50000}"#,
            r#"{"type":"widget","id":"c3","parentId":"b2","timestamp":"2026-02-14T09:21:00.000Z"}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.metadata["unrepresented"],
            "compaction 1, unrecognised:widget 1"
        );
    }

    #[test]
    fn reader_reports_nothing_when_every_record_was_accounted_for() {
        let session = read_clawdbot(&[
            r#"{"type":"session","version":2,"id":"s1","timestamp":"2026-02-14T09:12:03.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"go"}}"#,
        ]);
        assert!(
            session.metadata["unrepresented"].is_null(),
            "absent means nothing was dropped, never that the reader did not look"
        );
    }

    // -----------------------------------------------------------------------
    // Storage layout
    // -----------------------------------------------------------------------

    /// Both layouts exist on a real machine — the migration only runs when the
    /// gateway starts, and it leaves a `sessions.legacy-*` directory behind
    /// when it cannot move a file. Reading only one of them loses sessions
    /// silently.
    #[test]
    fn session_dirs_covers_both_the_agent_and_the_legacy_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        std::fs::create_dir_all(state.join("agents/main/sessions")).unwrap();
        std::fs::create_dir_all(state.join("agents/work/sessions")).unwrap();
        std::fs::create_dir_all(state.join("sessions")).unwrap();

        // `session_dirs` reads the environment, which cannot be mutated safely
        // in Rust 2024 tests, so the layout walk is asserted directly against
        // the same directory shape the resolver builds.
        let mut agent_dirs: Vec<PathBuf> = std::fs::read_dir(state.join("agents"))
            .unwrap()
            .flatten()
            .map(|e| e.path().join("sessions"))
            .filter(|p| p.is_dir())
            .collect();
        agent_dirs.sort();
        assert_eq!(agent_dirs.len(), 2, "one sessions dir per agent");
        assert!(agent_dirs[0].ends_with("agents/main/sessions"));
        assert!(agent_dirs[1].ends_with("agents/work/sessions"));
        assert!(state.join("sessions").is_dir(), "legacy layout still there");
    }

    // -----------------------------------------------------------------------
    // Writer
    // -----------------------------------------------------------------------

    fn sample_session(messages: Vec<CanonicalMessage>) -> CanonicalSession {
        CanonicalSession {
            session_id: "roundtrip-test".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(PathBuf::from("/home/u/proj")),
            title: Some("Test".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_001_000_000),
            messages,
            metadata: json!({"source": "claude-code"}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: Some("claude-sonnet-4-5".to_string()),
        }
    }

    fn msg(idx: usize, role: MessageRole, content: &str) -> CanonicalMessage {
        CanonicalMessage {
            idx,
            role,
            content: content.to_string(),
            timestamp: Some(1_700_000_000_000 + idx as i64 * 1000),
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        }
    }

    /// Render through the real writer, then land the bytes in an explicit
    /// directory, so the test never depends on `CLAWDBOT_HOME` (which cannot be
    /// set safely in Rust 2024 tests).
    fn write_to(dir: &Path, session: &CanonicalSession) -> PathBuf {
        let (content, _warnings) = ClawdBot::render(&session.session_id, session).unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join(format!("{}.jsonl", session.session_id));
        std::fs::write(&target, content).unwrap();
        target
    }

    #[test]
    fn writer_emits_the_session_manager_envelope() {
        let session = sample_session(vec![
            msg(0, MessageRole::User, "Fix the bug"),
            msg(1, MessageRole::Assistant, "I'll fix it now."),
        ]);
        let (text, warnings) = ClawdBot::render(&session.session_id, &session).unwrap();
        assert!(warnings.is_empty());
        let lines: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every written line must be JSON"))
            .collect();

        assert_eq!(lines.len(), 3, "header plus one entry per message");
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["id"], "roundtrip-test");
        assert_eq!(lines[0]["cwd"], "/home/u/proj");

        assert_eq!(lines[1]["type"], "message");
        assert_eq!(lines[1]["message"]["role"], "user");
        assert_eq!(lines[1]["message"]["content"], "Fix the bug");
        assert!(lines[1]["parentId"].is_null(), "first entry is the root");

        assert_eq!(lines[2]["message"]["role"], "assistant");
        assert_eq!(lines[2]["message"]["content"][0]["type"], "text");
        assert_eq!(
            lines[2]["parentId"], lines[1]["id"],
            "entries form the id/parentId chain SessionManager expects"
        );
        // pi's AssistantMessage requires these; a file without them is not one
        // the real agent can load.
        for field in ["api", "provider", "model", "usage", "stopReason"] {
            assert!(
                !lines[2]["message"][field].is_null(),
                "assistant entry must carry {field}"
            );
        }
    }

    #[test]
    fn writer_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let original = sample_session(vec![
            msg(0, MessageRole::User, "Fix the bug"),
            msg(1, MessageRole::Assistant, "I'll fix it now."),
            msg(2, MessageRole::Tool, "exit 0"),
            msg(3, MessageRole::System, "You are helpful."),
        ]);
        let path = write_to(tmp.path(), &original);

        let readback = ClawdBot.read_session(&path).unwrap();
        assert_eq!(readback.messages.len(), original.messages.len());
        for (orig, back) in original.messages.iter().zip(readback.messages.iter()) {
            assert_eq!(orig.role, back.role, "role must survive the round trip");
            assert_eq!(orig.content, back.content, "content must survive verbatim");
        }
        assert_eq!(
            readback.workspace.as_deref(),
            Some(Path::new("/home/u/proj"))
        );
    }

    #[test]
    fn writer_resume_command() {
        assert_eq!(
            ClawdBot.resume_command("my-session"),
            "clawdbot --resume my-session"
        );
    }

    // -----------------------------------------------------------------------
    // Provider metadata
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let provider = ClawdBot;
        assert_eq!(provider.name(), "ClawdBot");
        assert_eq!(provider.slug(), "clawdbot");
        assert_eq!(provider.cli_alias(), "cwb");
    }

    // NOTE: Detection tests that need env var mutation are skipped in Rust 2024
    // (set_var is unsafe). Detection is tested indirectly via json_contract_test.rs
    // which sets CLAWDBOT_HOME via process env before spawning.
}
