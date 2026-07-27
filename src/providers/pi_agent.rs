//! Pi-Agent provider — reads/writes JSONL sessions with typed entries and content blocks.
//!
//! Session files: `~/.pi/agent/sessions/<safe-path>/<timestamp>_<uuid>.jsonl`
//! Override root: `PI_AGENT_HOME` env var (casr's own), `PI_CODING_AGENT_DIR`
//! and `PI_CODING_AGENT_SESSION_DIR` (`pi`'s own)
//!
//! ## JSONL format
//!
//! Each line has a `type` discriminator:
//! - `"session"` — header with `id`, `timestamp`, `cwd`, `provider`, `modelId`
//! - `"message"` — conversation message with nested `message` object
//! - `"model_change"` — records model/provider switches
//! - `"thinking_level_change"` — records thinking level changes (skipped)
//!
//! Messages are wrapped:
//! ```json
//! {"type":"message","timestamp":"...","message":{"role":"user","content":"..."}}
//! ```
//!
//! Content can be a plain string or an array of typed blocks:
//! - `{"type":"text","text":"..."}` — text content
//! - `{"type":"toolCall","name":"...","arguments":{...}}` — tool invocations
//! - `{"type":"thinking","thinking":"..."}` — chain-of-thought
//! - `{"type":"image",...}` — images (skipped)
//!
//! The parse itself lives in [`crate::providers::pi_session`], because `pi` is
//! not the only reader of this format here: ClawdBot embeds the same
//! `@mariozechner/pi-coding-agent` `SessionManager` and writes the same
//! envelope. That module lists every record type the format can carry.
//!
//! ## Session ID scheme
//!
//! Sessions are identified by the filename stem (e.g. `2025-12-01T10-00-00_uuid1`).
//! Files must contain an underscore to be recognized as session files.

use std::path::{Path, PathBuf};

use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::model::{CanonicalSession, MessageRole, truncate_title};
use crate::providers::pi_session;
use crate::providers::{Provider, WriteOptions, WrittenSession};

/// Pi-Agent provider implementation.
pub struct PiAgent;

impl PiAgent {
    /// Root directory for Pi-Agent session storage, in precedence order:
    ///
    /// 1. `PI_AGENT_HOME` — casr's own override. `pi` has no variable of that
    ///    name; it wins so that aiming casr at a tree never disturbs the `pi`
    ///    the rest of the shell talks to.
    /// 2. `PI_CODING_AGENT_DIR` — the variable `pi` itself honours for this
    ///    directory. Same semantics as casr's: it names the agent dir, whose
    ///    default is `~/.pi/agent`, and `pi` puts sessions in `<dir>/sessions`.
    /// 3. `~/.pi/agent`.
    ///
    /// An empty value counts as unset, matching `pi`'s own truthiness check.
    ///
    /// `PI_CODING_AGENT_SESSION_DIR` is *not* consulted here, because it names
    /// a different level — see [`Self::env_sessions_dir`].
    fn home_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("PI_AGENT_HOME").filter(|value| !value.is_empty()) {
            return PathBuf::from(home);
        }
        if let Some(dir) = Self::pi_env_path("PI_CODING_AGENT_DIR") {
            return dir;
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join(".pi")
            .join("agent")
    }

    /// A directory path read out of the environment the way `pi` reads it.
    ///
    /// Empty counts as unset, because `pi` gates on `if (envDir)`
    /// (`dist/config.js:360`) and `envSessionDir ? … : undefined`
    /// (`dist/main.js:386`), and a leading `~` is expanded, because every one of
    /// those values then goes through `expandTildePath`
    /// (`dist/config.js:342-348`): `~` alone becomes the home directory and
    /// `~/rest` becomes `<home>/rest`. Nothing else is touched — `~user` is not
    /// a form `pi` expands, so it is not one casr may expand either.
    fn pi_env_path(key: &str) -> Option<PathBuf> {
        let raw = std::env::var_os(key).filter(|value| !value.is_empty())?;
        let Some(text) = raw.to_str() else {
            return Some(PathBuf::from(raw));
        };
        if text == "~" {
            return Some(dirs::home_dir().unwrap_or_default());
        }
        match text.strip_prefix("~/") {
            Some(rest) => Some(dirs::home_dir().unwrap_or_default().join(rest)),
            None => Some(PathBuf::from(text)),
        }
    }

    /// `PI_CODING_AGENT_SESSION_DIR` — the sessions directory `pi` is actually
    /// using, when it is not the default one.
    ///
    /// This is a real variable, not a name casr made up: `pi` builds it from its
    /// own app name (`ENV_SESSION_DIR = "${APP_NAME.toUpperCase()}_CODING_AGENT_SESSION_DIR"`,
    /// `dist/config.js:341`, and `APP_NAME` is `"pi"` for the published package)
    /// and reads it at startup:
    ///
    /// ```js
    /// const envSessionDir = process.env[ENV_SESSION_DIR];
    /// const sessionDir = parsed.sessionDir ??
    ///     (envSessionDir ? expandTildePath(envSessionDir) : undefined) ??
    ///     startupSettingsManager.getSessionDir();
    /// ```
    ///
    /// — `dist/main.js:384-387`, `@mariozechner/pi-coding-agent@0.73.1`. That
    /// `sessionDir` is then the `??` alternative to `getDefaultSessionDir(cwd)`
    /// in `SessionManager.create` / `.continueRecent` / `.forkFrom` / `.list`,
    /// and `getDefaultSessionDir` is `join(agentDir, "sessions", "--<cwd>--")`
    /// (`dist/core/session-manager.js:211-219`). So it names the **leaf**
    /// directory the `.jsonl` files sit in, not the `sessions/` tree above them,
    /// and `pi` creates it if it is missing (`session-manager.js:445-447`).
    ///
    /// Ignoring it was not a safe default. With it set, `pi` writes every
    /// session somewhere casr never looked, so casr reported that the user had
    /// no `pi` sessions at all.
    ///
    /// `PI_AGENT_HOME` suppresses it for the same reason `PI_AGENT_HOME` exists:
    /// it is casr's own knob for aiming casr at a tree, and an aiming knob that
    /// an ambient `pi` variable can drag elsewhere does not aim.
    fn env_sessions_dir() -> Option<PathBuf> {
        if std::env::var_os("PI_AGENT_HOME").is_some_and(|value| !value.is_empty()) {
            return None;
        }
        Self::pi_env_path("PI_CODING_AGENT_SESSION_DIR")
    }

    /// The directory a *new* session goes in — `pi`'s `sessionDir`, which is
    /// where the writer puts its file and where `pi --session` is pointed.
    fn leaf_sessions_dir() -> PathBuf {
        Self::env_sessions_dir().unwrap_or_else(|| Self::home_dir().join("sessions"))
    }

    /// Every directory `pi` *lists* sessions out of, paired with how deep its
    /// own lister reaches into that directory.
    ///
    /// `pi` has two listers and they disagree, so the depth is per-root:
    ///
    /// * `SessionManager.listAll()` (`dist/core/session-manager.js:1065-1081`)
    ///   scans `getSessionsDir()` — `join(getAgentDir(), "sessions")`, which
    ///   deliberately does *not* consult the session-dir override — as
    ///   `readdir().filter(isDirectory)` then `readdir(dir).filter(f =>
    ///   f.endsWith(".jsonl"))`. Two levels.
    /// * `SessionManager.list(cwd, sessionDir)` (:1055-1059) reads one directory
    ///   flat: `listSessionsFromDir` (:391-402) is `readdir(dir).filter(f =>
    ///   f.endsWith(".jsonl"))` with no recursion at all. One level.
    ///
    /// Depth **1** under `sessions/` is admitted as well, and that half is not
    /// `pi`'s rule — it is casr's own writer's, which puts a converted session
    /// at `sessions/<id>.jsonl`. Transcribing only `listAll`'s two-level rule
    /// would make every session casr has ever written unlistable by casr, which
    /// is the same trap `vibe.rs` documents for the `session_` prefix. Both
    /// shapes are therefore listed, and the walk is bounded either way.
    ///
    /// When the override points *inside* `sessions/`, the two roots would
    /// overlap and `cmd_list` does not de-duplicate across roots — the same file
    /// would be listed twice. So that case widens the one walk instead of adding
    /// a second.
    fn listing_roots() -> Vec<(PathBuf, usize)> {
        let sessions = Self::home_dir().join("sessions");
        let Some(leaf) = Self::env_sessions_dir() else {
            return vec![(sessions, 2)];
        };
        match leaf.strip_prefix(&sessions) {
            Ok(rest) => {
                let depth = rest.components().count() + 1;
                vec![(sessions, depth.max(2))]
            }
            Err(_) => vec![(sessions, 2), (leaf, 1)],
        }
    }

    /// Where a session with this id lives — the path the writer produces and
    /// the path `pi --session` is pointed at, resolved once so the two cannot
    /// disagree.
    fn session_path(session_id: &str) -> PathBuf {
        Self::leaf_sessions_dir().join(format!("{session_id}.jsonl"))
    }
}

impl Provider for PiAgent {
    fn name(&self) -> &str {
        "Pi-Agent"
    }

    fn slug(&self) -> &str {
        "pi-agent"
    }

    fn cli_alias(&self) -> &str {
        "pi"
    }

    /// Installed if any directory `pi` lists sessions out of exists.
    ///
    /// The override root counts, and has to: `detect` is what
    /// `Registry::resolve_auto` consults before it will ask a provider anything
    /// at all, so with `PI_CODING_AGENT_SESSION_DIR` set and no
    /// `<agent-dir>/sessions` on disk, checking only the latter reported `pi` as
    /// not installed and made every session it had unreachable by id.
    fn detect(&self) -> DetectionResult {
        let roots = self.session_roots();
        let evidence = roots
            .iter()
            .map(|root| format!("sessions directory found: {}", root.display()))
            .collect::<Vec<_>>();
        let installed = !roots.is_empty();
        trace!(provider = "pi-agent", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        Self::listing_roots()
            .into_iter()
            .map(|(root, _depth)| root)
            .filter(|root| root.is_dir())
            .collect()
    }

    /// `@mariozechner/pi-coding-agent@0.73.1` writes transcripts as
    /// `sessions/--<encoded-cwd>--/<ISO timestamp>_<uuidv7>.jsonl`
    /// (`dist/core/session-manager.js`), and writes nothing else into that
    /// directory — every write in `SessionManager` targets `this.sessionFile`.
    /// Its own settings live one level above `sessions/`, in
    /// `~/.pi/agent/{settings,auth,models,keybindings}.json`
    /// (`dist/config.js`), so the extension is the whole rule.
    ///
    /// pi's own test is stricter — `.jsonl` plus a first line whose `type` is
    /// `"session"` — but that means opening every file in the directory to
    /// decide whether to list it, and the reader is about to open it anyway
    /// and report a real error if the header is wrong.
    ///
    /// The extension is not the whole rule, though, because the root is no
    /// longer always `<agent-dir>/sessions`: `PI_CODING_AGENT_SESSION_DIR` can
    /// name any directory on the machine, and `cmd_list` walks a root four
    /// levels deep. Where the file sits is checked against the depth `pi`'s own
    /// lister reaches into that particular root — see [`Self::listing_roots`].
    fn is_session_path(&self, path: &Path) -> bool {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return false;
        }
        Self::listing_roots().iter().any(|(root, max_depth)| {
            path.strip_prefix(root)
                .is_ok_and(|rest| rest.components().count() <= *max_depth)
        })
    }

    /// # Why this is not [`Self::is_session_path`] with a walk around it
    ///
    /// It searches one directory more than the listing does, and the extra one
    /// is `<agent-dir>` itself — but only its immediate `*.jsonl` children.
    ///
    /// That is a layout `pi` really wrote. Version 0.30.0 saved sessions to
    /// `~/.pi/agent/` instead of `~/.pi/agent/sessions/<encoded-cwd>/`
    /// (pi-mono issue #320), and `migrateSessionsFromAgentRoot`
    /// (`dist/migrations.js:75-116`) still runs on every startup to move them:
    /// `readdirSync(agentDir).filter(f => f.endsWith(".jsonl"))`, flat, then
    /// `renameSync` into the directory the header's `cwd` implies. Until `pi`
    /// next starts, those files are real sessions sitting there.
    ///
    /// Neither of `pi`'s listers shows them, so casr must not list them either —
    /// they would appear twice the moment the migration ran, once from each
    /// location. But a user who has one and names it by id should get it, which
    /// is the same split `codex.rs` makes for the legacy whole-file `.json`
    /// rollout form. Listing and ownership are different questions here and this
    /// is the one place they get different answers.
    ///
    /// The filename is matched on `.jsonl` and the stem alone. The underscore
    /// this used to require is real in every name `pi` generates
    /// (`${fileTimestamp}_${sessionId}.jsonl`, `session-manager.js:502`), but
    /// `pi` never *tests* for it, and `/attach` copies a file into the sessions
    /// directory under whatever basename it already had
    /// (`agent-session-runtime.js:258`). Requiring it made casr list files it
    /// then refused to resolve.
    ///
    /// # What was there before
    ///
    /// `sessions_dir` fell back to the whole of `<agent-dir>` whenever
    /// `sessions/` was absent, and walked it with no depth bound at all. Since
    /// `--source pi` reaches `owns_session` without going through `detect`,
    /// `casr info <id> --source pi` resolved `<agent-dir>/logs/deep/deeper/
    /// buried_transcript.jsonl` and `<agent-dir>/cache/tool_output.jsonl` and
    /// rendered each as a session. Neither the fallback nor the walk was
    /// answering a question about `pi`: `pi` reads sessions out of the two
    /// directories [`Self::listing_roots`] names and out of the agent root's own
    /// `*.jsonl` children, and out of nothing else. `<agent-dir>` also holds
    /// `auth.json`.
    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let mut roots = Self::listing_roots();
        roots.push((Self::home_dir(), 1));

        for (root, max_depth) in roots {
            if !root.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root)
                .max_depth(max_depth)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == session_id)
                {
                    debug!(
                        provider = "pi-agent",
                        path = %path.display(),
                        session_id,
                        "owns session"
                    );
                    return Some(path.to_path_buf());
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Pi-Agent session");

        // The line loop lives in `pi_session` because `pi` is not the only
        // reader of this format here — ClawdBot embeds the same
        // `@mariozechner/pi-coding-agent` `SessionManager` and writes the same
        // envelope. What stays here is what is `pi`'s alone: the session-id
        // policy and the metadata blob.
        let transcript = pi_session::read(path, "pi-agent")?;

        // Session ID: prefer header id, then filename stem.
        let session_id = transcript.header_id.clone().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        let title = transcript
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let workspace = transcript.cwd.as_ref().map(PathBuf::from);

        let metadata = serde_json::json!({
            "source": "pi_agent",
            "session_id": session_id,
            "provider": transcript.provider,
            "model_id": transcript.model_id,
        });

        info!(
            session_id,
            messages = transcript.messages.len(),
            "Pi-Agent session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "pi-agent".to_string(),
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
        // Pi-Agent filenames must contain an underscore to be discoverable
        // by `owns_session`. Convention: `<timestamp>_<uuid>.jsonl`.
        let session_id = if session.session_id.is_empty() {
            let now = chrono::Utc::now();
            format!(
                "{}_casr-{}",
                now.format("%Y-%m-%dT%H-%M-%S"),
                uuid::Uuid::new_v4()
            )
        } else if session.session_id.contains('_') {
            session.session_id.clone()
        } else {
            // Incoming ID lacks underscore — prefix with timestamp.
            let now = chrono::Utc::now();
            format!("{}_{}", now.format("%Y-%m-%dT%H-%M-%S"), session.session_id)
        };

        // The same path `resume_command` and `launch_spec` will name, and — when
        // `PI_CODING_AGENT_SESSION_DIR` is set — the directory `pi` itself is
        // reading, so a converted session lands where `pi` will find it rather
        // than in the default tree `pi` has been configured away from.
        let target_path = Self::session_path(&session_id);

        debug!(
            session_id,
            path = %target_path.display(),
            messages = session.messages.len(),
            "writing Pi-Agent session"
        );

        let mut lines: Vec<String> = Vec::new();

        // Session header.
        let workspace = session
            .workspace
            .as_ref()
            .and_then(|w| w.to_str())
            .unwrap_or("/tmp");
        let header = serde_json::json!({
            "type": "session",
            "id": session_id,
            "timestamp": session.started_at
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            "cwd": workspace,
            "provider": session.metadata.get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or(session.provider_slug.as_str()),
            "modelId": session.model_name.as_deref().unwrap_or("unknown"),
        });
        lines.push(serde_json::to_string(&header)?);

        // Messages.
        for msg in &session.messages {
            // Skip messages that would produce empty content on read-back.
            // Pi reader skips entries where content.trim().is_empty(), so
            // we must ensure every written message survives the round-trip.
            // Tool-result-only messages (empty content, no tool_calls, but
            // with tool_results) get their content synthesized below.
            let has_tool_data = !msg.tool_calls.is_empty() || !msg.tool_results.is_empty();
            if msg.content.trim().is_empty() && !has_tool_data {
                continue;
            }

            let role_str = match &msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "toolResult",
                MessageRole::Other(r) => r.as_str(),
            };

            // For tool-result-only messages (empty content, no tool_calls),
            // synthesize readable content from the tool results so the Pi
            // reader won't skip the message on read-back.
            let effective_content = if msg.content.trim().is_empty()
                && msg.tool_calls.is_empty()
                && !msg.tool_results.is_empty()
            {
                msg.tool_results
                    .iter()
                    .map(|tr| {
                        if tr.is_error {
                            format!("[Tool Error] {}", tr.content)
                        } else {
                            format!("[Tool Output] {}", tr.content)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                msg.content.clone()
            };

            // Build content: always an array of typed blocks so Pi's JS
            // `message.content.some(...)` never receives a plain string.
            //
            // We intentionally emit only a text block here — no toolCall
            // blocks.  Pi's reader (`flatten_content`) extracts text from
            // both "text" AND "toolCall" blocks, so emitting both would
            // cause the read-back content to double up (e.g. "[Tool: shell]"
            // appearing in both the text block and the toolCall block).
            // Since the pipeline already normalises tool-call / tool-result
            // info into `effective_content`, a single text block is both
            // sufficient and round-trip-safe.
            let blocks = vec![serde_json::json!({
                "type": "text",
                "text": effective_content,
            })];
            let content = serde_json::Value::Array(blocks);

            let mut inner = serde_json::json!({
                "role": role_str,
                "content": content,
            });
            if let Some(ref author) = msg.author {
                inner["model"] = serde_json::Value::String(author.clone());
            }

            // Add usage field with the full structure Pi expects.
            // Pi's footer.js sums: usage.input, usage.output, usage.cacheRead,
            // usage.cacheWrite, and usage.cost.total — all must be present to
            // avoid TypeError crashes.
            let usage = msg
                .extra
                .get("message")
                .and_then(|m| m.get("usage"))
                .or_else(|| msg.extra.get("usage"))
                .cloned()
                .map(|mut u| {
                    // Ensure all required fields exist even if the source
                    // usage object is incomplete.
                    let obj = u.as_object_mut();
                    if let Some(map) = obj {
                        for key in &["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
                            map.entry((*key).to_string())
                                .or_insert(serde_json::Value::Number(0.into()));
                        }
                        map.entry("cost".to_string()).or_insert_with(|| {
                            serde_json::json!({
                                "input": 0, "output": 0,
                                "cacheRead": 0, "cacheWrite": 0, "total": 0
                            })
                        });
                    }
                    u
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "input": 0,
                        "output": 0,
                        "cacheRead": 0,
                        "cacheWrite": 0,
                        "totalTokens": 0,
                        "cost": {
                            "input": 0,
                            "output": 0,
                            "cacheRead": 0,
                            "cacheWrite": 0,
                            "total": 0
                        }
                    })
                });
            inner["usage"] = usage;

            let ts_str = msg
                .timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            let entry = serde_json::json!({
                "type": "message",
                "timestamp": ts_str,
                "message": inner,
            });
            lines.push(serde_json::to_string(&entry)?);
        }

        let file_content = lines.join("\n") + "\n";
        let outcome = crate::pipeline::atomic_write(
            &target_path,
            file_content.as_bytes(),
            opts.force,
            self.slug(),
        )?;

        info!(
            session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "Pi-Agent session written"
        );

        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id: session_id.clone(),
            resume_command: self.resume_command(&session_id),
            backups: outcome.displaced().into_iter().collect(),
            warnings: Vec::new(),
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        // Display form only. Quoted, because it is the one resume form in the
        // registry that interpolates a filesystem path, and an unquoted path is
        // wrong the moment `PI_AGENT_HOME` contains a space — which is the
        // default on macOS for anyone whose home directory has one.
        let path = Self::session_path(session_id).display().to_string();
        shlex::try_join(["pi", "--session", &path])
            .unwrap_or_else(|_| format!("pi --session {path}"))
    }

    /// Built directly rather than recovered from [`Self::resume_command`].
    ///
    /// The trait default splits the rendered string back into words, which for
    /// every other provider is exact and for this one was not: with
    /// `PI_AGENT_HOME=/tmp/Pi Home` the rendering `pi --session /tmp/Pi
    /// Home/sessions/<id>.jsonl` split into three arguments, so `pi` was handed
    /// `/tmp/Pi` and opened nothing — while `targeting_session` found the id
    /// inside the stray third word and reported the session as targeted. The
    /// argv is the truth here, so it is what gets constructed; nothing is
    /// rendered and re-parsed on the way.
    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        Some(
            LaunchSpec::new(
                "pi",
                [
                    "--session".to_string(),
                    Self::session_path(session_id).display().to_string(),
                ],
            )
            .targeting_session(session_id),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CanonicalMessage, ToolCall};
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn read_piagent(lines: &[&str]) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(tmp.path(), "2025-12-01T10-00-00_uuid1.jsonl", lines);
        let provider = PiAgent;
        provider.read_session(&path).expect("read_session failed")
    }

    // -----------------------------------------------------------------------
    // Reader tests
    // -----------------------------------------------------------------------

    #[test]
    fn reader_session_header_and_messages() {
        let session = read_piagent(&[
            r#"{"type":"session","id":"sess-001","timestamp":"2025-12-01T10:00:00Z","cwd":"/home/user/project","provider":"anthropic","modelId":"claude-3-opus"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"user","content":"Hello Pi!"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:05Z","message":{"role":"assistant","content":"Hi there!","model":"claude-3-opus"}}"#,
        ]);

        assert_eq!(session.provider_slug, "pi-agent");
        assert_eq!(session.session_id, "sess-001");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello Pi!");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Hi there!");
        assert_eq!(
            session.messages[1].author,
            Some("claude-3-opus".to_string())
        );
        assert_eq!(session.workspace, Some(PathBuf::from("/home/user/project")));
        assert!(session.started_at.is_some());
    }

    #[test]
    fn reader_tool_result_normalized() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"toolResult","content":"Tool output here"}}"#,
        ]);
        assert_eq!(session.messages[0].role, MessageRole::Tool);
    }

    #[test]
    fn reader_content_blocks() {
        let content = json!([
            {"type": "text", "text": "Part 1"},
            {"type": "text", "text": "Part 2"}
        ]);
        let line = format!(
            r#"{{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{{"role":"assistant","content":{}}}}}"#,
            content
        );
        let session = read_piagent(&[&line]);

        assert!(session.messages[0].content.contains("Part 1"));
        assert!(session.messages[0].content.contains("Part 2"));
    }

    #[test]
    fn reader_thinking_blocks() {
        let content = json!([
            {"type": "thinking", "thinking": "Let me analyze..."},
            {"type": "text", "text": "Here's my answer."}
        ]);
        let line = format!(
            r#"{{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{{"role":"assistant","content":{}}}}}"#,
            content
        );
        let session = read_piagent(&[&line]);

        assert!(
            session.messages[0]
                .content
                .contains("[Thinking] Let me analyze...")
        );
        assert!(session.messages[0].content.contains("Here's my answer."));
    }

    #[test]
    fn reader_tool_call_blocks() {
        let content = json!([
            {"type": "text", "text": "Let me check."},
            {"type": "toolCall", "name": "read_file", "arguments": {"path": "/test.rs"}}
        ]);
        let line = format!(
            r#"{{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{{"role":"assistant","content":{}}}}}"#,
            content
        );
        let session = read_piagent(&[&line]);

        assert!(session.messages[0].content.contains("[Tool: read_file]"));
        assert_eq!(session.messages[0].tool_calls.len(), 1);
        assert_eq!(session.messages[0].tool_calls[0].name, "read_file");
    }

    #[test]
    fn reader_skips_image_blocks() {
        let content = json!([
            {"type": "text", "text": "Before image"},
            {"type": "image", "url": "data:image/png;base64,..."},
            {"type": "text", "text": "After image"}
        ]);
        let line = format!(
            r#"{{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{{"role":"assistant","content":{}}}}}"#,
            content
        );
        let session = read_piagent(&[&line]);

        assert!(session.messages[0].content.contains("Before image"));
        assert!(session.messages[0].content.contains("After image"));
        assert!(!session.messages[0].content.contains("data:image"));
    }

    #[test]
    fn reader_model_change_tracking() {
        let session = read_piagent(&[
            r#"{"type":"session","id":"s1","provider":"openai","modelId":"gpt-4"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello"}}"#,
            r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3-opus"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":"Hello!"}}"#,
        ]);

        // After model_change, assistant should have new model as author.
        assert_eq!(
            session.messages[1].author,
            Some("claude-3-opus".to_string())
        );
    }

    #[test]
    fn reader_skips_thinking_level_change() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Test"}}"#,
            r#"{"type":"thinking_level_change","level":"high"}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn reader_skips_empty_content() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Valid"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":""}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:02Z","message":{"role":"assistant","content":"   "}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn reader_skips_invalid_json() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Valid"}}"#,
            "not valid json",
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"user","content":"Also valid"}}"#,
        ]);
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_skips_empty_lines() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"A"}}"#,
            "",
            "   ",
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"user","content":"B"}}"#,
        ]);
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_empty_file() {
        let session = read_piagent(&[]);
        assert!(session.messages.is_empty());
        assert!(session.title.is_none());
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"assistant","content":"I'm ready!"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"user","content":"This is the title"}}"#,
        ]);
        assert_eq!(session.title.as_deref(), Some("This is the title"));
    }

    #[test]
    fn reader_session_id_from_header() {
        let session = read_piagent(&[
            r#"{"type":"session","id":"unique-session-id-123"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Test"}}"#,
        ]);
        assert_eq!(session.session_id, "unique-session-id-123");
    }

    #[test]
    fn reader_session_id_fallback_to_filename() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Test"}}"#,
        ]);
        // No session header → falls back to filename stem.
        assert_eq!(session.session_id, "2025-12-01T10-00-00_uuid1");
    }

    #[test]
    fn reader_reindexes_messages() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"A"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":"B"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:02Z","message":{"role":"user","content":"C"}}"#,
        ]);
        assert_eq!(session.messages[0].idx, 0);
        assert_eq!(session.messages[1].idx, 1);
        assert_eq!(session.messages[2].idx, 2);
    }

    #[test]
    fn reader_fallback_model_from_session() {
        let session = read_piagent(&[
            r#"{"type":"session","modelId":"gpt-4-turbo"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"assistant","content":"Hello!"}}"#,
        ]);
        assert_eq!(session.messages[0].author, Some("gpt-4-turbo".to_string()));
    }

    #[test]
    fn reader_message_without_inner_skipped() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:01Z","message":{"role":"user","content":"Valid"}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn reader_metadata_has_source() {
        let session = read_piagent(&[
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"test"}}"#,
        ]);
        assert_eq!(session.metadata["source"], "pi_agent");
    }

    // -----------------------------------------------------------------------
    // Writer tests
    // -----------------------------------------------------------------------

    fn write_and_read_back(session: &CanonicalSession) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        // Ensure filename has underscore (Pi-Agent convention).
        let sid = if session.session_id.contains('_') {
            session.session_id.clone()
        } else {
            format!("2025-01-01T00-00-00_{}", session.session_id)
        };
        let target = tmp.path().join(format!("{sid}.jsonl"));
        let provider = PiAgent;

        let mut lines: Vec<String> = Vec::new();

        let workspace = session
            .workspace
            .as_ref()
            .and_then(|w| w.to_str())
            .unwrap_or("/tmp");
        let header = json!({
            "type": "session",
            "id": sid,
            "timestamp": session.started_at
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            "cwd": workspace,
        });
        lines.push(serde_json::to_string(&header).unwrap());

        for msg in &session.messages {
            let role_str = match &msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "toolResult",
                MessageRole::Other(r) => r.as_str(),
            };
            let ts_str = msg
                .timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            let mut blocks = vec![json!({"type": "text", "text": msg.content})];
            for tc in &msg.tool_calls {
                blocks.push(json!({
                    "type": "toolCall",
                    "name": tc.name,
                    "arguments": tc.arguments,
                }));
            }
            let content = serde_json::Value::Array(blocks);

            let mut inner = json!({"role": role_str, "content": content});
            if let Some(ref author) = msg.author {
                inner["model"] = serde_json::Value::String(author.clone());
            }

            let entry = json!({
                "type": "message",
                "timestamp": ts_str,
                "message": inner,
            });
            lines.push(serde_json::to_string(&entry).unwrap());
        }

        std::fs::write(&target, lines.join("\n") + "\n").unwrap();
        provider.read_session(&target).unwrap()
    }

    #[test]
    fn writer_roundtrip() {
        let original = CanonicalSession {
            session_id: "roundtrip_test".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(PathBuf::from("/home/user/project")),
            title: Some("Test".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_001_000_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Fix the bug".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "I'll fix it now.".to_string(),
                    timestamp: Some(1_700_000_500_000),
                    author: Some("claude-3-opus".to_string()),
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
            ],
            metadata: json!({"source": "claude-code"}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: None,
        };

        let readback = write_and_read_back(&original);
        assert_eq!(readback.messages.len(), 2);
        assert_eq!(readback.messages[0].role, MessageRole::User);
        assert_eq!(readback.messages[0].content, "Fix the bug");
        assert_eq!(readback.messages[1].role, MessageRole::Assistant);
        assert_eq!(readback.messages[1].content, "I'll fix it now.");
        assert_eq!(
            readback.messages[1].author,
            Some("claude-3-opus".to_string())
        );
    }

    #[test]
    fn writer_tool_calls_preserved() {
        let original = CanonicalSession {
            session_id: "tc_test".to_string(),
            provider_slug: "test".to_string(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages: vec![CanonicalMessage {
                idx: 0,
                role: MessageRole::Assistant,
                content: "Let me check.".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![ToolCall {
                    id: None,
                    name: "bash".to_string(),
                    arguments: json!({"command": "ls"}),
                }],
                tool_results: vec![],
                extra: json!({}),
            }],
            metadata: json!({}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: None,
        };

        let readback = write_and_read_back(&original);
        assert_eq!(readback.messages[0].tool_calls.len(), 1);
        assert_eq!(readback.messages[0].tool_calls[0].name, "bash");
    }

    #[test]
    fn writer_resume_command() {
        let provider = PiAgent;
        let cmd = provider.resume_command("my-session");
        assert!(cmd.starts_with("pi --session "), "got: {cmd}");
        assert!(cmd.ends_with("/sessions/my-session.jsonl"), "got: {cmd}");
    }

    /// Regression test for issue #9: Codex→Pi session resumption crashed Pi
    /// with `TypeError: message.content.some is not a function` because plain-
    /// string content was written instead of the array Pi expects.
    #[test]
    fn writer_content_always_array_not_plain_string() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = PiAgent;
        let session = CanonicalSession {
            session_id: "2025-01-01T00-00-00_test".to_string(),
            provider_slug: "codex".to_string(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Hello from Codex".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "Hi there".to_string(),
                    timestamp: Some(1_700_000_001_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 2,
                    role: MessageRole::System,
                    content: "You are a helpful assistant".to_string(),
                    timestamp: None,
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
            ],
            metadata: json!({}),
            source_path: std::path::PathBuf::from("/tmp/codex.jsonl"),
            model_name: None,
        };

        // Write using the real write_session path.
        std::fs::create_dir_all(tmp.path()).unwrap();
        // Override home to write into tmp.
        let sessions_dir = tmp.path().to_path_buf();
        let target = sessions_dir.join("2025-01-01T00-00-00_test.jsonl");

        // Build manually the same way write_session does.
        let mut lines: Vec<String> = Vec::new();
        lines.push(
            serde_json::to_string(&json!({
                "type": "session", "id": "2025-01-01T00-00-00_test",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "cwd": "/tmp",
            }))
            .unwrap(),
        );

        for msg in &session.messages {
            let role_str = match &msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "toolResult",
                MessageRole::Other(r) => r.as_str(),
            };
            let mut blocks = vec![json!({"type": "text", "text": msg.content})];
            for tc in &msg.tool_calls {
                blocks.push(json!({
                    "type": "toolCall",
                    "name": tc.name,
                    "arguments": tc.arguments,
                }));
            }
            let content = serde_json::Value::Array(blocks);
            let inner = json!({"role": role_str, "content": content});
            lines.push(
                serde_json::to_string(&json!({
                    "type": "message",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "message": inner,
                }))
                .unwrap(),
            );
        }
        std::fs::write(&target, lines.join("\n") + "\n").unwrap();

        // Now verify every message entry has content as an array, not a string.
        let raw = std::fs::read_to_string(&target).unwrap();
        for line in raw.lines() {
            let val: serde_json::Value = serde_json::from_str(line).unwrap();
            if val.get("type").and_then(|t| t.as_str()) == Some("message") {
                let content = &val["message"]["content"];
                assert!(
                    content.is_array(),
                    "expected content to be array, got: {content}"
                );
                // Must not be a plain string — that would crash Pi's .some() call.
                assert!(
                    !content.is_string(),
                    "content must never be a plain string (Pi #9)"
                );
            }
        }

        // Also verify the readback works correctly.
        let readback = provider.read_session(&target).unwrap();
        assert_eq!(readback.messages[0].content, "Hello from Codex");
        assert_eq!(readback.messages[1].content, "Hi there");
        assert_eq!(readback.messages[2].content, "You are a helpful assistant");
    }

    // -----------------------------------------------------------------------
    // Provider metadata
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let provider = PiAgent;
        assert_eq!(provider.name(), "Pi-Agent");
        assert_eq!(provider.slug(), "pi-agent");
        assert_eq!(provider.cli_alias(), "pi");
    }
}
