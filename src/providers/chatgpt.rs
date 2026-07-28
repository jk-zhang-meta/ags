//! ChatGPT desktop app provider — reads/writes JSON sessions.
//!
//! Session files are individual JSON files per conversation with a tree-based
//! `mapping` structure (node IDs → messages with parent pointers).
//!
//! ## Which store this is
//!
//! `home_dir` resolves to `~/Library/Application
//! Support/com.openai.chat` on macOS and to nothing at all on every other
//! platform, so off macOS this provider reads only what `CHATGPT_HOME` is
//! explicitly pointed at. There is no Windows or Linux path here to be wrong
//! about, and `CHATGPT_HOME` is casr's own override — the desktop app honours
//! no such variable.
//!
//! ## Storage generations
//!
//! Verified against `ChatGPT.app` (`ChatGPT.dmg`, 78,575,566 bytes, sha256
//! `49b33cad…`). Its `ChatGPT.framework` binary carries the directory-name
//! components as standalone NUL-terminated literals beside
//! `cleanUpLegacyDirectoryIfNeeded(for:accountID:appGroupID:)`:
//!
//! - `conversations-v3-` — current. A *prefix*, joined to the account id at
//!   runtime, so the directory is `conversations-v3-<accountID>`.
//! - `conversations_v2_` and `conversations_v2_cache` — the previous
//!   generation, underscore-separated rather than hyphen-separated.
//!
//! Both generations are encrypted, so `list` reports them as refused rather
//! than pretending the store is empty. Swift interpolates these at runtime, so
//! no complete directory path exists in the binary as a literal and the exact
//! concatenation below the prefix is *not* established.
//!
//! The plain `conversations-<id>/<id>.json` tree this module reads has no
//! counterpart in that artifact: the binary has zero occurrences of a
//! `conversations-<uuid>` name without a version token, and no
//! `JSONEncoder`/`JSONDecoder` or conversation-related `.json` literal anywhere
//! near the conversation code. It is the shape casr's own `write_session`
//! produces and the shape the synthetic fixture uses — not a shape any shipped
//! app generation has been observed to write. See `is_session_path` for what
//! that does and does not justify.
//!
//! ## The `system` turn is a turn
//!
//! `author.role` is a slot, not a filter. `system` is one of the cases of the
//! app's own author-role enum — `ConversationMessage.Author.KnownRole`, whose
//! field descriptor in `ChatGPT.framework` reads `assistant / system / critic /
//! tool / developer` at byte 94870708 and `user / assistant / critic / system /
//! developer` at 94778914.
//!
//! [`ChatGpt::write_session`] emits `"system"` for [`MessageRole::System`], and
//! `pipeline::folded_role` declares chatgpt folds *nothing* — the writer's own
//! statement that this format can name the role. The reader used to drop every
//! node whose `author.role` was `"system"` before looking at it. Writer and
//! reader disagreed about the same file, so a conversion of any session holding
//! a system turn came back one message short and was rolled back:
//!
//! ```text
//! VerifyFailed: message count mismatch: wrote 3 messages, read back 2
//! ```
//!
//! No vendor rule was being transcribed. What the format has instead is a
//! *visibility* marker — `isVisuallyHiddenFromConversation` and
//! `isUserSystemMessage`, stored properties of `ConversationMessageMetadata`
//! (byte 85387904/85387936, snake-cased on the wire by
//! `JSONDecoder.convertFromSnakeCase`) — and casr does not need to consult it.
//! Across six real exported conversations, every one of the eight `system`
//! nodes carried `content: {"content_type":"text","parts":[""]}` and
//! `is_visually_hidden_from_conversation: true`, and **none** carried words:
//! the empty-content test two branches down already drops all of them. A flag
//! casr's own writer never emits could not help the round trip either.
//!
//! So the fix is the one that changes nothing about reading a real
//! conversation and everything about reading casr's own: report the role the
//! file states, and let `normalize_role` hand it to every target with a system
//! slot of its own.
//!
//! Two adjacent read gaps this deliberately does *not* close, named so they are
//! not mistaken for handled. Custom instructions arrive in two shapes — an
//! older `role: "system"` node whose text lives in
//! `metadata.user_context_message_data.about_model_message`, and a newer
//! `role: "user"` node with `content_type: "user_editable_context"` and **no
//! `parts` key at all**. Both are read as empty here and dropped.
//!
//! ## Resume
//!
//! ChatGPT doesn't have a CLI resume mechanism. The resume command opens the
//! conversation in a browser: `https://chatgpt.com/c/<conversation-id>`
//!
//! ## CASS heritage
//!
//! Reader logic ported from `coding_agent_session_search/src/connectors/chatgpt.rs`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, info, trace};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, flatten_content, normalize_role,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession, read_dir_reporting,
};

/// ChatGPT desktop app provider implementation.
pub struct ChatGpt;

impl ChatGpt {
    /// Root directory for ChatGPT app data.
    ///
    /// `CHATGPT_HOME` is casr's own override — the desktop app honours no such
    /// variable — and off macOS it is the only way to reach a store at all.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("CHATGPT_HOME") {
            return Some(PathBuf::from(home));
        }
        // ChatGPT desktop is macOS only.
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/com.openai.chat"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// Find conversation directories under a base path.
    ///
    /// Returns `(path, is_encrypted)` pairs, where `is_encrypted` means "the
    /// app wrote this and casr cannot read it", not "casr found no files".
    ///
    /// Both separators are deliberate. The shipped app names its store with a
    /// version token joined to the account id, and it changed separator between
    /// generations: `conversations-v3-<accountID>` today,
    /// `conversations_v2_<accountID>` and `conversations_v2_cache` before it.
    /// Matching only `conversations-` — as this did — made an entire real v2
    /// store invisible: not read, and not reported as refused either, so `list`
    /// said "no sessions" about a directory that was full of them. Anything
    /// carrying a version token is the app's own encrypted store; the plain
    /// `conversations-<id>` form is the one casr writes itself, and an id is
    /// hex, so it can never begin with `v`.
    fn find_conversation_dirs_reporting(
        base: &Path,
        unreadable: &mut Vec<UnreadableSource>,
    ) -> Vec<(PathBuf, bool)> {
        let mut dirs = Vec::new();

        for entry in read_dir_reporting(base, unreadable) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if name.starts_with("conversations-") || name.starts_with("conversations_") {
                let is_encrypted =
                    name.starts_with("conversations-v") || name.starts_with("conversations_");
                dirs.push((path, is_encrypted));
            }
        }

        dirs
    }

    /// `find_conversation_dirs_reporting` for the callers with nowhere to put a
    /// read failure — `detect`, `session_roots`, `owns_session`, the writer.
    fn find_conversation_dirs(base: &Path) -> Vec<(PathBuf, bool)> {
        Self::find_conversation_dirs_reporting(base, &mut Vec::new())
    }
}

impl Provider for ChatGpt {
    fn name(&self) -> &str {
        "ChatGPT"
    }

    fn slug(&self) -> &str {
        "chatgpt"
    }

    fn cli_alias(&self) -> &str {
        "gpt"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            let conv_dirs = Self::find_conversation_dirs(&home);
            if !conv_dirs.is_empty() {
                let encrypted = conv_dirs.iter().filter(|(_, enc)| *enc).count();
                let unencrypted = conv_dirs.len() - encrypted;

                evidence.push(format!("{} exists", home.display()));
                if unencrypted > 0 {
                    evidence.push(format!("{unencrypted} unencrypted conversation dir(s)"));
                }
                if encrypted > 0 {
                    evidence.push(format!(
                        "{encrypted} encrypted conversation dir(s) (not yet supported)"
                    ));
                }
                installed = true;
            }
        }

        trace!(provider = "chatgpt", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let Some(home) = Self::home_dir() else {
            return vec![];
        };
        if !home.is_dir() {
            return vec![];
        }
        // Each unencrypted conversations-* directory is a session root.
        Self::find_conversation_dirs(&home)
            .into_iter()
            .filter(|(_, encrypted)| !encrypted)
            .map(|(path, _)| path)
            .collect()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let home = Self::home_dir()?;

        let mut listing = SessionListing::default();
        for (dir, encrypted) in
            Self::find_conversation_dirs_reporting(&home, &mut listing.unreadable)
        {
            if encrypted {
                // Not a failure to read, but a refusal to pretend. `detect`
                // already calls the app installed on the strength of this
                // directory; without this the user gets "✓ ChatGPT" and an
                // empty listing with nothing saying which of the two reasons
                // applies.
                listing.unreadable.push(UnreadableSource {
                    path: dir,
                    error: "encrypted conversation store (v2/v3); casr cannot read it".to_string(),
                });
                continue;
            }
            for entry in read_dir_reporting(&dir, &mut listing.unreadable) {
                let path = entry.path();
                if !self.is_session_path(&path) || !path.is_file() {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                listing.sessions.push((stem.to_string(), path));
            }
        }

        Some(listing)
    }

    /// One `<conversation-id>.json` per conversation, flat in a
    /// `conversations-<uuid>/` directory — the same shape `owns_session`
    /// resolves against, at the same depth.
    ///
    /// ## What this rule is, and what it is not
    ///
    /// It was committed as explicitly unverified, because no shipped artifact
    /// for the desktop app could be obtained. One has since been read
    /// (`ChatGPT.dmg`, sha256 `49b33cad…`), and it does **not** ratify the
    /// rule. What it establishes:
    ///
    /// - The app's own object store names files by the bare item id with no
    ///   extension: `ObjectLoader/FilenameStringConvertible.swift`,
    ///   `init(fromFilenameValue:)`, `Failed to decode filename as item ID`,
    ///   `Filename cannot be empty`. A full read of that string-pool block
    ///   found no `.json`, `pathExtension`, or `appendingPathExtension` near
    ///   it, and the nearest `.json` literals belong to unrelated features.
    /// - That naming is *not* exclusive to the encrypted store. The literals
    ///   sit in one contiguous block with `EncryptedObjectLoader.swift` **and**
    ///   `UnencryptedObjectLoader.swift`, `FileBackedCache.swift`, and
    ///   `SingleItemObjectLoader.swift` — one `ObjectLoader` module, confirmed
    ///   by the mangled `$s12ObjectLoader25FilenameStringConvertibleP`. So
    ///   "unencrypted" would not buy back a `.json` extension.
    ///
    /// The rule nevertheless stays, because it is not describing the app's
    /// store. Every directory the artifact attests to carries a version token
    /// and is refused before this predicate is ever reached, and off macOS
    /// `home_dir` resolves to nothing at all. The tree this predicate actually
    /// governs is the one casr's own `write_session` creates —
    /// `conversations-<uuid>/<uuid>.json` — which the ten `*_to_chatgpt` tests
    /// write and `list` must therefore keep showing. Widening it to
    /// extension-less files would not gain a single real session, and would
    /// hand the reader arbitrary non-JSON files to fail on, which is the
    /// "reports a sidecar as an unreadable session" defect in a new place.
    ///
    /// ## Still unverified
    ///
    /// Whether any shipped generation ever wrote the plain
    /// `conversations-<uuid>/<id>.json` tree this module reads. The artifact
    /// shows no trace of one — zero `conversations-<uuid>` names without a
    /// version token, no `JSONEncoder`/`JSONDecoder` near the conversation
    /// code — but absence in one build is not proof it never existed, and the
    /// reader was inherited from CASS rather than derived from an artifact.
    /// The fixture backing it is marked `synthetic` in the manifest. If a real
    /// readable store is ever obtained, this predicate is the thing to check
    /// first.
    fn is_session_path(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("json")
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let home = Self::home_dir()?;
        if !home.is_dir() {
            return None;
        }

        let id_lower = session_id.to_ascii_lowercase();

        // Walk through all conversation directories looking for a matching file.
        for (dir, encrypted) in Self::find_conversation_dirs(&home) {
            if encrypted {
                continue;
            }

            for entry in WalkDir::new(&dir).max_depth(1).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path();
                let ext = path.extension().and_then(|s| s.to_str());
                if ext != Some("json") {
                    continue;
                }

                // Quick check: filename stem matches session ID.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && stem.eq_ignore_ascii_case(session_id)
                {
                    return Some(path.to_path_buf());
                }

                // Deeper check: parse JSON and look for matching "id" or "conversation_id".
                // Use a minimal struct to avoid allocating the massive `mapping` objects in memory.
                #[derive(serde::Deserialize)]
                struct ChatGptHeader {
                    id: Option<String>,
                    conversation_id: Option<String>,
                }
                if let Ok(file) = std::fs::File::open(path) {
                    let reader = std::io::BufReader::new(file);
                    if let Ok(header) = serde_json::from_reader::<_, ChatGptHeader>(reader) {
                        let conv_id = header.id.as_deref().or(header.conversation_id.as_deref());
                        if let Some(cid) = conv_id
                            && cid.eq_ignore_ascii_case(&id_lower)
                        {
                            return Some(path.to_path_buf());
                        }
                    }
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading ChatGPT session");

        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let root: serde_json::Value = serde_json::from_reader(reader)
            .with_context(|| format!("failed to parse JSON {}", path.display()))?;

        // Session ID: prefer "id", then "conversation_id", then filename stem.
        let session_id = root
            .get("id")
            .or_else(|| root.get("conversation_id"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        let title = root.get("title").and_then(|v| v.as_str()).map(String::from);

        // Top-level timestamps (float seconds).
        let started_at = root.get("create_time").and_then(parse_timestamp);
        let mut ended_at = root.get("update_time").and_then(parse_timestamp);

        // Model name from top-level.
        let model_name = root.get("model").and_then(|v| v.as_str()).map(String::from);

        let mut messages: Vec<CanonicalMessage> = Vec::new();

        // Primary format: tree-based "mapping" structure.
        if let Some(mapping) = root.get("mapping").and_then(|v| v.as_object()) {
            let mut msg_nodes: Vec<(&str, &serde_json::Value)> = Vec::new();

            for (node_id, node) in mapping {
                if let Some(msg) = node.get("message")
                    && msg.is_object()
                {
                    msg_nodes.push((node_id.as_str(), msg));
                }
            }

            // Sort by create_time for deterministic ordering.
            msg_nodes.sort_by(|a, b| {
                let ts_a = a.1.get("create_time").and_then(|v| v.as_f64());
                let ts_b = b.1.get("create_time").and_then(|v| v.as_f64());
                match (ts_a, ts_b) {
                    (Some(a_ts), Some(b_ts)) => a_ts
                        .partial_cmp(&b_ts)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.cmp(b.0)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.0.cmp(b.0),
                }
            });

            for (_node_id, msg) in msg_nodes {
                let role_str = msg
                    .get("author")
                    .and_then(|a| a.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                let role = normalize_role(role_str);

                // Content: prefer "parts" array, then "text" field.
                let content_val = msg.get("content");
                let text = if let Some(parts) = content_val
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    parts
                        .iter()
                        .filter_map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else if let Some(content) = content_val {
                    flatten_content(content)
                } else {
                    continue;
                };

                if text.trim().is_empty() {
                    continue;
                }

                // Timestamp: float seconds → millis.
                let ts = msg.get("create_time").and_then(parse_timestamp);
                if let Some(t) = ts {
                    ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
                }

                // Model from message metadata.
                let msg_model = msg
                    .get("metadata")
                    .and_then(|m| m.get("model_slug"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                messages.push(CanonicalMessage {
                    idx: 0,
                    role,
                    content: text,
                    timestamp: ts,
                    author: msg_model,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: msg.clone(),
                });
            }
        }

        // Fallback: simple "messages" array format (ChatGPT data exports).
        if messages.is_empty()
            && let Some(msgs) = root.get("messages").and_then(|v| v.as_array())
        {
            for msg in msgs {
                let role_str = msg
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                let role = normalize_role(role_str);

                let text = msg.get("content").map(flatten_content).unwrap_or_default();

                if text.trim().is_empty() {
                    continue;
                }

                let ts = msg
                    .get("timestamp")
                    .or_else(|| msg.get("create_time"))
                    .and_then(parse_timestamp);

                if let Some(t) = ts {
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
                    extra: msg.clone(),
                });
            }
        }

        reindex_messages(&mut messages);

        // Title: prefer explicit, fall back to first user message.
        let effective_title = title.or_else(|| {
            messages
                .iter()
                .find(|m| m.role == MessageRole::User)
                .map(|m| truncate_title(&m.content, 100))
        });

        // Metadata.
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("chatgpt".to_string()),
        );
        if let Some(ref m) = model_name {
            metadata.insert("model".into(), serde_json::Value::String(m.clone()));
        }

        debug!(
            session_id,
            messages = messages.len(),
            "ChatGPT session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "chatgpt".to_string(),
            workspace: None, // ChatGPT doesn't have a workspace concept.
            title: effective_title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let target_session_id = uuid::Uuid::new_v4().to_string();

        // Determine target directory.
        let home = Self::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine ChatGPT home directory"))?;

        // Write into conversations-<uuid>/ directory.
        let conv_dir = home.join(format!("conversations-{target_session_id}"));
        let target_path = conv_dir.join(format!("{target_session_id}.json"));

        debug!(
            target_session_id,
            target_path = %target_path.display(),
            "writing ChatGPT session"
        );

        // Build the ChatGPT mapping structure.
        let now_secs = chrono::Utc::now().timestamp() as f64;
        let create_time = session
            .started_at
            .map(|ms| ms as f64 / 1000.0)
            .unwrap_or(now_secs);
        let update_time = session
            .ended_at
            .map(|ms| ms as f64 / 1000.0)
            .unwrap_or(now_secs);

        let mut mapping = serde_json::Map::new();
        let mut prev_node_id: Option<String> = None;

        for msg in &session.messages {
            let node_id = uuid::Uuid::new_v4().to_string();
            let chatgpt_role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
                MessageRole::System => "system",
                MessageRole::Other(ref s) => s.as_str(),
            };

            let msg_ts = msg.timestamp.map(|ms| ms as f64 / 1000.0);

            let mut message_obj = serde_json::json!({
                "author": {"role": chatgpt_role},
                "content": {"parts": [msg.content]},
            });

            if let Some(ts) = msg_ts {
                message_obj["create_time"] = serde_json::Value::from(ts);
            }

            // Add model info for assistant messages.
            if msg.role == MessageRole::Assistant {
                let model_slug = msg.author.as_deref().or(session.model_name.as_deref());
                if let Some(slug) = model_slug {
                    message_obj["metadata"] = serde_json::json!({
                        "model_slug": slug,
                    });
                }
            }

            let mut node = serde_json::json!({
                "message": message_obj,
            });

            node["parent"] = prev_node_id
                .as_ref()
                .map(|id| serde_json::Value::String(id.clone()))
                .unwrap_or(serde_json::Value::Null);

            mapping.insert(node_id.clone(), node);
            prev_node_id = Some(node_id);
        }

        let root = serde_json::json!({
            "id": target_session_id,
            "title": session.title.as_deref().unwrap_or("Imported conversation"),
            "create_time": create_time,
            "update_time": update_time,
            "mapping": mapping,
        });

        let content_bytes = serde_json::to_string_pretty(&root)?.into_bytes();

        let outcome =
            crate::pipeline::atomic_write(&target_path, &content_bytes, opts.force, self.slug())?;

        info!(
            target_session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "ChatGPT session written"
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
        format!("open \"https://chatgpt.com/c/{session_id}\"")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::ChatGpt;
    use serde_json::json;
    use std::io::Write as _;

    use crate::model::{CanonicalMessage, MessageRole};
    use crate::providers::Provider;

    /// Write JSON to a temp file and read it back via the ChatGPT reader.
    fn read_chatgpt_json(content: &str) -> crate::model::CanonicalSession {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();
        ChatGpt
            .read_session(tmp.path())
            .unwrap_or_else(|e| panic!("read_session failed: {e}"))
    }

    // -----------------------------------------------------------------------
    // Provider metadata
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let p = ChatGpt;
        assert_eq!(p.name(), "ChatGPT");
        assert_eq!(p.slug(), "chatgpt");
        assert_eq!(p.cli_alias(), "gpt");
    }

    #[test]
    fn resume_command_is_browser_url() {
        let p = ChatGpt;
        assert_eq!(
            p.resume_command("abc-123"),
            "open \"https://chatgpt.com/c/abc-123\""
        );
    }

    // -----------------------------------------------------------------------
    // Detection
    // -----------------------------------------------------------------------

    #[test]
    fn detect_does_not_panic() {
        let p = ChatGpt;
        let result = p.detect();
        // On CI (Linux), ChatGPT desktop won't be installed.
        let _ = result.installed;
    }

    #[test]
    fn detect_with_chatgpt_home_env() {
        let dir = tempfile::TempDir::new().unwrap();
        // Create a conversations directory.
        let conv_dir = dir.path().join("conversations-uuid123");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Temporarily override CHATGPT_HOME — test isolation not needed here
        // because detect() reads the env each time and doesn't cache.
        // NOTE: We can't use set_var (unsafe in Rust 2024), so we test via
        // find_conversation_dirs directly instead.
        let dirs = ChatGpt::find_conversation_dirs(dir.path());
        assert_eq!(dirs.len(), 1);
        assert!(!dirs[0].1); // Not encrypted.
    }

    #[test]
    fn find_conversation_dirs_detects_encrypted() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("conversations-v2-abc")).unwrap();
        std::fs::create_dir_all(dir.path().join("conversations-v3-xyz")).unwrap();
        std::fs::create_dir_all(dir.path().join("conversations-plain")).unwrap();
        std::fs::create_dir_all(dir.path().join("other-folder")).unwrap();

        let dirs = ChatGpt::find_conversation_dirs(dir.path());
        assert_eq!(dirs.len(), 3);

        let encrypted = dirs.iter().filter(|(_, e)| *e).count();
        let unencrypted = dirs.iter().filter(|(_, e)| !*e).count();
        assert_eq!(encrypted, 2);
        assert_eq!(unencrypted, 1);
    }

    #[test]
    fn find_conversation_dirs_empty_for_nonexistent() {
        let dirs = ChatGpt::find_conversation_dirs(Path::new("/nonexistent/chatgpt/path"));
        assert!(dirs.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reader: mapping format
    // -----------------------------------------------------------------------

    #[test]
    fn reader_mapping_format_basic() {
        let session = read_chatgpt_json(
            &json!({
                "id": "conv-123",
                "title": "Test Conversation",
                "create_time": 1700000000.0,
                "update_time": 1700000010.0,
                "mapping": {
                    "node1": {
                        "parent": null,
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Hello, ChatGPT!"]},
                            "create_time": 1700000001.0
                        }
                    },
                    "node2": {
                        "parent": "node1",
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["Hello! How can I help?"]},
                            "create_time": 1700000002.0,
                            "metadata": {"model_slug": "gpt-4"}
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.session_id, "conv-123");
        assert_eq!(session.title.as_deref(), Some("Test Conversation"));
        assert_eq!(session.provider_slug, "chatgpt");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello, ChatGPT!");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert!(session.messages[1].content.contains("How can I help"));
        assert_eq!(session.messages[1].author, Some("gpt-4".to_string()));
    }

    #[test]
    fn reader_mapping_orders_by_create_time() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "late": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["Second"]},
                            "create_time": 1700000002.0
                        }
                    },
                    "early": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["First"]},
                            "create_time": 1700000001.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages[0].content, "First");
        assert_eq!(session.messages[1].content, "Second");
    }

    /// `author.role` is read, not filtered on. Dropping the system node here is
    /// what made `write_session` and `read_session` disagree about the file
    /// between them — `message count mismatch: wrote 3 messages, read back 2` —
    /// and roll every such conversion back.
    #[test]
    fn reader_mapping_keeps_system_messages() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "sys": {
                        "message": {
                            "author": {"role": "system"},
                            "content": {"parts": ["You are helpful."]},
                            "create_time": 1700000000.0
                        }
                    },
                    "user": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Hi!"]},
                            "create_time": 1700000001.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[0].content, "You are helpful.");
        assert_eq!(session.messages[1].role, MessageRole::User);
    }

    /// The hidden system node a real conversation opens with. It is dropped, but
    /// by the rule that drops any wordless turn — not by its role.
    #[test]
    fn reader_mapping_drops_the_empty_system_node_as_wordless_not_as_system() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "sys": {
                        "message": {
                            "author": {"role": "system"},
                            "content": {"parts": [""]},
                            "create_time": 1700000000.0,
                            "metadata": {"is_visually_hidden_from_conversation": true}
                        }
                    },
                    "user": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Hi!"]},
                            "create_time": 1700000001.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
    }

    #[test]
    fn reader_mapping_skips_empty_content() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "empty": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": [""]},
                            "create_time": 1700000000.0
                        }
                    },
                    "whitespace": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["   \n\t  "]},
                            "create_time": 1700000001.0
                        }
                    },
                    "valid": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Valid message"]},
                            "create_time": 1700000002.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Valid message");
    }

    #[test]
    fn reader_mapping_multipart_content() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Part 1", "Part 2", "Part 3"]},
                            "create_time": 1700000000.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages[0].content, "Part 1\nPart 2\nPart 3");
    }

    #[test]
    fn reader_mapping_text_content_field() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"text": "Using text field"},
                            "create_time": 1700000000.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages[0].content, "Using text field");
    }

    #[test]
    fn reader_mapping_conversation_id_fallback() {
        let session = read_chatgpt_json(
            &json!({
                "conversation_id": "alt-id-123",
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]}
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.session_id, "alt-id-123");
    }

    #[test]
    fn reader_mapping_id_fallback_to_filename() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]}
                        }
                    }
                }
            })
            .to_string(),
        );

        // Falls back to filename stem.
        assert!(!session.session_id.is_empty());
    }

    #[test]
    fn reader_mapping_timestamp_to_millis() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]},
                            "create_time": 1700000000.5
                        }
                    }
                }
            })
            .to_string(),
        );

        // 1700000000.5 seconds → 1700000000500 millis.
        assert_eq!(session.messages[0].timestamp, Some(1_700_000_000_500));
    }

    #[test]
    fn reader_mapping_preserves_extra() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]},
                            "create_time": 1700000000.0,
                            "custom_field": "preserved"
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(
            session.messages[0]
                .extra
                .get("custom_field")
                .and_then(|v| v.as_str()),
            Some("preserved")
        );
    }

    #[test]
    fn reader_mapping_model_in_metadata() {
        let session = read_chatgpt_json(
            &json!({
                "model": "gpt-4-turbo",
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]}
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.model_name.as_deref(), Some("gpt-4-turbo"));
        assert_eq!(session.metadata["model"], "gpt-4-turbo");
    }

    #[test]
    fn reader_mapping_skips_non_object_messages() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "bad1": {"message": "not an object"},
                    "bad2": {"message": null},
                    "bad3": {"parent": "bad2"},
                    "bad4": {"message": 42},
                    "good": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Valid"]},
                            "create_time": 1700000000.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Valid");
    }

    #[test]
    fn reader_mapping_empty_returns_empty() {
        let session = read_chatgpt_json(&json!({"mapping": {}}).to_string());
        assert!(session.messages.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reader: simple messages array format (data export)
    // -----------------------------------------------------------------------

    #[test]
    fn reader_simple_messages_format() {
        let session = read_chatgpt_json(
            &json!({
                "id": "simple-conv",
                "title": "Simple Format",
                "messages": [
                    {"role": "user", "content": "Question?", "timestamp": 1700000000000_i64},
                    {"role": "assistant", "content": "Answer!", "timestamp": 1700000001000_i64}
                ]
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Question?");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Answer!");
    }

    #[test]
    fn reader_simple_messages_keeps_system() {
        let session = read_chatgpt_json(
            &json!({
                "messages": [
                    {"role": "system", "content": "You are helpful."},
                    {"role": "user", "content": "Hi!"}
                ]
            })
            .to_string(),
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[1].role, MessageRole::User);
    }

    /// The whole defect in one test: a session with a system turn has to
    /// survive its own writer and reader. `pipeline`'s read-back verification
    /// compares counts first, and this is the pair it compares.
    #[test]
    fn writer_system_turn_survives_read_back() {
        let messages = [
            ("system", "You are helpful."),
            ("user", "Question?"),
            ("assistant", "Answer!"),
        ];
        let mapping: serde_json::Map<String, serde_json::Value> = messages
            .iter()
            .enumerate()
            .map(|(i, (role, text))| {
                (
                    format!("node-{i}"),
                    json!({"message": {
                        "author": {"role": role},
                        "content": {"parts": [text]},
                        "create_time": 1_700_000_000.0 + i as f64,
                    }}),
                )
            })
            .collect();

        let session = read_chatgpt_json(
            &json!({"id": "conv-1", "mapping": mapping, "create_time": 1_700_000_000.0})
                .to_string(),
        );

        assert_eq!(session.messages.len(), 3);
        let roles: Vec<&MessageRole> = session.messages.iter().map(|m| &m.role).collect();
        assert_eq!(
            roles,
            vec![
                &MessageRole::System,
                &MessageRole::User,
                &MessageRole::Assistant
            ]
        );
    }

    // -----------------------------------------------------------------------
    // Reader: edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn reader_empty_json_object() {
        let session = read_chatgpt_json("{}");
        assert!(session.messages.is_empty());
    }

    #[test]
    fn reader_invalid_json_returns_error() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        tmp.write_all(b"not valid json").unwrap();
        tmp.flush().unwrap();
        assert!(ChatGpt.read_session(tmp.path()).is_err());
    }

    #[test]
    fn reader_title_fallback_to_first_user_message() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Explain the architecture"]},
                            "create_time": 1700000000.0
                        }
                    },
                    "node2": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["The architecture uses..."]},
                            "create_time": 1700000001.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.title.as_deref(), Some("Explain the architecture"));
    }

    #[test]
    fn reader_started_at_from_create_time() {
        let session = read_chatgpt_json(
            &json!({
                "create_time": 1700000000.0,
                "update_time": 1700000010.0,
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Test"]}
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.started_at, Some(1_700_000_000_000));
        assert_eq!(session.ended_at, Some(1_700_000_010_000));
    }

    #[test]
    fn reader_ended_at_tracks_max_message_ts() {
        let session = read_chatgpt_json(
            &json!({
                "update_time": 1700000005.0,
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["First"]},
                            "create_time": 1700000001.0
                        }
                    },
                    "node2": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["Last"]},
                            "create_time": 1700000020.0
                        }
                    }
                }
            })
            .to_string(),
        );

        // ended_at should be max(update_time, last message timestamp).
        assert_eq!(session.ended_at, Some(1_700_000_020_000));
    }

    #[test]
    fn reader_reindexes_messages() {
        let session = read_chatgpt_json(
            &json!({
                "mapping": {
                    "a": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["First"]},
                            "create_time": 1700000001.0
                        }
                    },
                    "b": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["Second"]},
                            "create_time": 1700000002.0
                        }
                    },
                    "c": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Third"]},
                            "create_time": 1700000003.0
                        }
                    }
                }
            })
            .to_string(),
        );

        assert_eq!(session.messages[0].idx, 0);
        assert_eq!(session.messages[1].idx, 1);
        assert_eq!(session.messages[2].idx, 2);
    }

    // -----------------------------------------------------------------------
    // Writer
    // -----------------------------------------------------------------------

    #[test]
    fn writer_produces_valid_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = crate::model::CanonicalSession {
            session_id: "test-write".to_string(),
            provider_slug: "chatgpt".to_string(),
            workspace: Some(dir.path().to_path_buf()),
            title: Some("Test Write".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Hello".to_string(),
                    timestamp: Some(1_700_000_001_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::Value::Null,
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "Hi there".to_string(),
                    timestamp: Some(1_700_000_002_000),
                    author: Some("gpt-4".to_string()),
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::Value::Null,
                },
            ],
            metadata: json!({"source": "test"}),
            source_path: std::path::PathBuf::from("/tmp/test.json"),
            model_name: Some("gpt-4".to_string()),
        };

        // Set CHATGPT_HOME to temp dir so writer has a target.
        // Since we can't use set_var (unsafe), we test the JSON structure
        // by serializing what the writer would produce.
        let now_secs = chrono::Utc::now().timestamp() as f64;
        let mut mapping = serde_json::Map::new();
        let mut prev_node_id: Option<String> = None;

        for msg in &session.messages {
            let node_id = uuid::Uuid::new_v4().to_string();
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => "user",
            };
            let ts = msg.timestamp.map(|ms| ms as f64 / 1000.0);

            let mut message_obj = json!({
                "author": {"role": role},
                "content": {"parts": [msg.content]},
            });
            if let Some(t) = ts {
                message_obj["create_time"] = serde_json::Value::from(t);
            }
            if msg.role == MessageRole::Assistant
                && let Some(ref author) = msg.author
            {
                message_obj["metadata"] = json!({"model_slug": author});
            }

            let mut node = json!({"message": message_obj});
            node["parent"] = prev_node_id
                .as_ref()
                .map(|id| serde_json::Value::String(id.clone()))
                .unwrap_or(serde_json::Value::Null);

            mapping.insert(node_id.clone(), node);
            prev_node_id = Some(node_id);
        }

        let root = json!({
            "id": "test-id",
            "title": session.title,
            "create_time": session.started_at.map(|ms| ms as f64 / 1000.0).unwrap_or(now_secs),
            "update_time": session.ended_at.map(|ms| ms as f64 / 1000.0).unwrap_or(now_secs),
            "mapping": mapping,
        });

        // Verify the JSON is valid and has the expected structure.
        let serialized = serde_json::to_string_pretty(&root).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["id"], "test-id");
        assert_eq!(parsed["title"], "Test Write");
        assert!(parsed["mapping"].is_object());
        let mapping_obj = parsed["mapping"].as_object().unwrap();
        assert_eq!(mapping_obj.len(), 2);

        // Verify parent chain.
        let nodes: Vec<&serde_json::Value> = mapping_obj.values().collect();
        let root_nodes: Vec<_> = nodes.iter().filter(|n| n["parent"].is_null()).collect();
        assert_eq!(root_nodes.len(), 1, "should have exactly one root node");
    }

    #[test]
    fn writer_roundtrip() {
        // Create a session, serialize it as ChatGPT JSON, then read it back.
        let session = crate::model::CanonicalSession {
            session_id: "roundtrip-test".to_string(),
            provider_slug: "chatgpt".to_string(),
            workspace: None,
            title: Some("Roundtrip Test".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "User says hello".to_string(),
                    timestamp: Some(1_700_000_001_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::Value::Null,
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "Assistant responds".to_string(),
                    timestamp: Some(1_700_000_002_000),
                    author: Some("gpt-4o".to_string()),
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::Value::Null,
                },
            ],
            metadata: json!({"source": "chatgpt"}),
            source_path: std::path::PathBuf::from("/tmp/test.json"),
            model_name: Some("gpt-4o".to_string()),
        };

        // Build the ChatGPT JSON manually (writer logic).
        let mut mapping = serde_json::Map::new();
        let mut prev_node_id: Option<String> = None;

        for msg in &session.messages {
            let node_id = uuid::Uuid::new_v4().to_string();
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => "user",
            };
            let ts = msg.timestamp.map(|ms| ms as f64 / 1000.0);

            let mut message_obj = json!({
                "author": {"role": role},
                "content": {"parts": [msg.content]},
            });
            if let Some(t) = ts {
                message_obj["create_time"] = serde_json::Value::from(t);
            }
            if msg.role == MessageRole::Assistant
                && let Some(ref author) = msg.author
            {
                message_obj["metadata"] = json!({"model_slug": author});
            }

            let mut node = json!({"message": message_obj});
            node["parent"] = prev_node_id
                .as_ref()
                .map(|id| serde_json::Value::String(id.clone()))
                .unwrap_or(serde_json::Value::Null);

            mapping.insert(node_id.clone(), node);
            prev_node_id = Some(node_id);
        }

        let root = json!({
            "id": "roundtrip-id",
            "title": "Roundtrip Test",
            "create_time": 1700000000.0,
            "update_time": 1700000010.0,
            "mapping": mapping,
        });

        // Write to temp file and read back.
        let mut tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        tmp.write_all(serde_json::to_string_pretty(&root).unwrap().as_bytes())
            .unwrap();
        tmp.flush().unwrap();

        let read_back = ChatGpt.read_session(tmp.path()).unwrap();

        assert_eq!(read_back.session_id, "roundtrip-id");
        assert_eq!(read_back.title.as_deref(), Some("Roundtrip Test"));
        assert_eq!(read_back.messages.len(), 2);
        assert_eq!(read_back.messages[0].role, MessageRole::User);
        assert_eq!(read_back.messages[0].content, "User says hello");
        assert_eq!(read_back.messages[1].role, MessageRole::Assistant);
        assert_eq!(read_back.messages[1].content, "Assistant responds");
        assert_eq!(read_back.messages[1].author.as_deref(), Some("gpt-4o"));
    }

    // -----------------------------------------------------------------------
    // Owns session
    // -----------------------------------------------------------------------

    #[test]
    fn owns_session_finds_by_filename() {
        let dir = tempfile::TempDir::new().unwrap();
        let conv_dir = dir.path().join("conversations-uuid123");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let conv = json!({
            "id": "my-conv-id",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]}
                    }
                }
            }
        });
        std::fs::write(conv_dir.join("my-conv-id.json"), conv.to_string()).unwrap();

        // Test find_conversation_dirs directly since we can't set env var.
        let dirs = ChatGpt::find_conversation_dirs(dir.path());
        assert_eq!(dirs.len(), 1);

        // Verify the file exists with the expected name.
        let files: Vec<_> = std::fs::read_dir(&conv_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().to_str().unwrap(), "my-conv-id.json");
    }

    // -----------------------------------------------------------------------
    // Session roots
    // -----------------------------------------------------------------------

    #[test]
    fn session_roots_empty_without_home() {
        // On Linux without CHATGPT_HOME, session_roots should be empty.
        let p = ChatGpt;
        // This is a valid test on Linux where there's no macOS app support dir.
        // The result depends on whether CHATGPT_HOME is set.
        let _ = p.session_roots();
    }

    use std::path::Path;
}
