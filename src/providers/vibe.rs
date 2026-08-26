//! Vibe (Mistral) provider — reads/writes JSONL chat sessions.
//!
//! Session files: `~/.vibe/logs/session/session_<UTC>_<id8>/messages.jsonl`
//! Override root: `VIBE_HOME` env var
//!
//! ## JSONL format
//!
//! Vibe uses a flexible JSONL message format where role, content, and timestamp
//! may appear under several different field names:
//!
//! - Role: `role`, `speaker`, or nested `message.role`
//! - Content: `content`, `text`, or nested `message.content`
//! - Timestamp: `timestamp`, `created_at`, `createdAt`, `time`, `ts`
//!
//! ## Session ID scheme
//!
//! Sessions live in `session_<UTC>_<id8>` subdirectories. `meta.json` carries
//! the full session ID; the directory's suffix is only its first eight bytes.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, flatten_content, normalize_role,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession, filename_safe_session_id};

/// Vibe provider implementation.
pub struct Vibe;

const MESSAGES_FILENAME: &str = "messages.jsonl";
const METADATA_FILENAME: &str = "meta.json";
const DEFAULT_SESSION_PREFIX: &str = "session";
const SHORT_ID_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
type VendorCandidate = (PathBuf, Option<String>, std::time::SystemTime);

impl Vibe {
    /// Directory holding Vibe's session logs.
    ///
    /// `VIBE_HOME` is Vibe's own variable and means what Vibe means by it: the
    /// `~/.vibe` root, not the session-log directory. Vibe's README — "By
    /// default, Vibe stores its configuration in `~/.vibe/`. You can override
    /// this by setting the `VIBE_HOME` environment variable" — lists `logs/`
    /// as one of the subdirectories under that root, and session logs live in
    /// `<root>/logs/session`. So `logs/session` is joined onto it, exactly as
    /// Vibe does.
    ///
    /// An empty value counts as unset.
    fn home_dir() -> PathBuf {
        let root = match std::env::var_os("VIBE_HOME").filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => dirs::home_dir().unwrap_or_default().join(".vibe"),
        };
        root.join("logs").join("session")
    }

    /// Read the sidecar Vibe uses to identify and describe a session.
    fn session_metadata(session_dir: &Path) -> Option<Value> {
        let bytes = std::fs::read(session_dir.join(METADATA_FILENAME)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn metadata_session_id(metadata: &Value) -> Option<&str> {
        metadata
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
    }

    /// The extra condition Vibe's `list_sessions` applies after its structural
    /// log check. Full metadata validation happens only when Vibe resumes the
    /// selected session; reproducing Pydantic's schema here would be a second,
    /// inevitably drifting oracle.
    fn metadata_is_listable(metadata: &Value) -> bool {
        metadata.is_object() && Self::metadata_session_id(metadata).is_some()
    }

    /// Whether Vibe's resolver considers this a structurally valid log. This
    /// is deliberately looser than both `list_sessions` and Pydantic metadata
    /// validation: direct `--resume ID` selects from this set.
    fn is_vendor_log_dir(session_dir: &Path) -> bool {
        let Some(name) = session_dir.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if !name.starts_with("session_") {
            return false;
        }

        let Some(metadata) = Self::session_metadata(session_dir) else {
            return false;
        };
        if !metadata.is_object() {
            return false;
        }

        let Ok(content) = std::fs::read_to_string(session_dir.join(MESSAGES_FILENAME)) else {
            return false;
        };
        let mut lines: Vec<&str> = content.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        if lines.is_empty() && metadata.get("total_messages").and_then(Value::as_u64) != Some(0) {
            return false;
        }

        lines
            .iter()
            .all(|line| matches!(serde_json::from_str::<Value>(line), Ok(Value::Object(_))))
    }

    /// Whether Vibe's default-config loader admits this directory to its
    /// session list. The writer emits the complete `SessionMetadata` schema;
    /// this predicate intentionally mirrors the vendor's looser list oracle.
    fn is_vendor_session_dir(session_dir: &Path) -> bool {
        Self::is_vendor_log_dir(session_dir)
            && Self::session_metadata(session_dir)
                .as_ref()
                .is_some_and(Self::metadata_is_listable)
    }

    /// Encode 47 digest bits as eight alphanumeric base62 characters. Vibe
    /// resolves a session using only these first eight characters, so a
    /// conventional hex prefix would leave just 32 bits to distinguish
    /// imported sessions. Keeping the prefix alphanumeric also makes the
    /// returned id unambiguous as an argument to `vibe --resume`.
    fn short_session_id(source_id: &str) -> String {
        let digest = Sha256::digest(source_id.as_bytes());
        let mut value = digest[..6]
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
            >> 1;
        let mut encoded = [SHORT_ID_ALPHABET[0]; 8];
        for byte in encoded.iter_mut().rev() {
            *byte = SHORT_ID_ALPHABET[(value % 62) as usize];
            value /= 62;
        }
        encoded.into_iter().map(char::from).collect()
    }

    /// Give the target a high-entropy native short prefix while retaining the
    /// source id in filename-safe form for provenance and diagnostics.
    fn native_session_id(source_id: &str) -> String {
        format!(
            "{}-{}",
            Self::short_session_id(source_id),
            filename_safe_session_id(source_id)
        )
    }

    /// Return every structurally valid directory Vibe's short-id glob can
    /// select, including logs whose metadata has no usable full id.
    fn vendor_candidates(short_id: &str) -> std::io::Result<Vec<VendorCandidate>> {
        let root = Self::home_dir();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let prefix = format!("{DEFAULT_SESSION_PREFIX}_");
        let suffix = format!("_{short_id}");
        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry?;
            let session_dir = entry.path();
            let Some(name) = session_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with(&prefix)
                || !name.ends_with(&suffix)
                || !Self::is_vendor_log_dir(&session_dir)
            {
                continue;
            }
            let Some(metadata) = Self::session_metadata(&session_dir) else {
                continue;
            };
            let full_id = Self::metadata_session_id(&metadata).map(str::to_owned);
            let messages_path = session_dir.join(MESSAGES_FILENAME);
            let Ok(modified) =
                std::fs::metadata(&messages_path).and_then(|metadata| metadata.modified())
            else {
                continue;
            };
            candidates.push((session_dir, full_id, modified));
        }
        Ok(candidates)
    }

    /// Select the same exact-id incarnation that Vibe would prefer, but only
    /// when the complete short-prefix set is unambiguous. Returning a path
    /// from a mixed set would claim a session that `vibe --resume` may not open.
    fn latest_session_dir_for_id(session_id: &str) -> Option<PathBuf> {
        let short_id: String = session_id.chars().take(8).collect();
        let candidates = Self::vendor_candidates(&short_id).ok()?;
        if candidates
            .iter()
            .any(|(_, full_id, _)| full_id.as_deref() != Some(session_id))
        {
            return None;
        }
        candidates
            .into_iter()
            .max_by_key(|(_, _, modified)| *modified)
            .map(|(session_dir, _, _)| session_dir)
    }

    /// Extract role from a JSONL line, checking multiple field names.
    fn extract_role(val: &serde_json::Value) -> String {
        val.get("role")
            .and_then(|v| v.as_str())
            .or_else(|| val.get("speaker").and_then(|v| v.as_str()))
            .or_else(|| {
                val.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("assistant")
            .to_string()
    }

    /// Extract content from a JSONL line, checking multiple field names.
    fn extract_content(val: &serde_json::Value) -> String {
        if let Some(content) = val.get("content") {
            return flatten_content(content);
        }
        if let Some(content) = val.get("text") {
            return flatten_content(content);
        }
        if let Some(content) = val.get("message").and_then(|msg| msg.get("content")) {
            return flatten_content(content);
        }
        String::new()
    }

    /// Extract timestamp from a JSONL line, checking multiple field names.
    fn extract_timestamp(val: &serde_json::Value) -> Option<i64> {
        let candidates = ["timestamp", "created_at", "createdAt", "time", "ts"];

        for key in candidates {
            if let Some(ts) = val.get(key).and_then(parse_timestamp) {
                return Some(ts);
            }
        }

        if let Some(message) = val.get("message") {
            for key in candidates {
                if let Some(ts) = message.get(key).and_then(parse_timestamp) {
                    return Some(ts);
                }
            }
        }

        None
    }
}

impl Provider for Vibe {
    fn name(&self) -> &str {
        "Vibe"
    }

    fn slug(&self) -> &str {
        "vibe"
    }

    fn cli_alias(&self) -> &str {
        "vib"
    }

    fn detect(&self) -> DetectionResult {
        let root = Self::home_dir();
        let installed = root.is_dir();
        let evidence = if installed {
            vec![format!("sessions directory found: {}", root.display())]
        } else {
            vec![]
        };
        trace!(provider = "vibe", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let root = Self::home_dir();
        if root.is_dir() { vec![root] } else { vec![] }
    }

    /// A Vibe session is a default-config `session_*` directory admitted by
    /// the vendor's list oracle. The vendor uses a direct `glob`, so only
    /// children of `logs/session/` participate.
    ///
    /// Named exactly, because the siblings are all things a looser rule admits:
    /// `meta.json` beside the transcript (rendered as an empty session), the
    /// `.last_session/<tty>` pointer directory, `attachments/<sha1><ext>`, and
    /// the `*.json.tmp` / `*.jsonl.tmp` atomic-write temporaries — of which
    /// only the first kind is ever swept, so `messages.jsonl.tmp` accumulates.
    ///
    /// # Where the directory has to be
    ///
    /// One level under `logs/session/`, because that is all Vibe looks at:
    /// `SessionLoader.list_sessions` is
    ///
    /// ```python
    /// pattern = f"{config.session_prefix}_*"
    /// session_dirs = list(save_dir.glob(pattern))
    /// ```
    ///
    /// — `glob`, not `rglob`, and no `**` in the pattern. The walk feeding this
    /// predicate is recursive (`main.rs`, `max_depth(4)`), so the depth rule has
    /// to be here.
    ///
    /// What it excludes is not a hypothetical. `vibe/core/tools/builtins/task.py`
    /// gives every subagent its own session logger rooted *inside* the parent
    /// session — `save_dir=str(ctx.session_dir / "agents")` — so
    /// `session_<stamp>/agents/<agent>_<stamp>/messages.jsonl` is a real
    /// transcript Vibe writes and never lists. ags was listing it as a peer of
    /// the session that spawned it.
    ///
    fn is_session_path(&self, path: &Path) -> bool {
        if path.file_name().and_then(|n| n.to_str()) != Some(MESSAGES_FILENAME) {
            return false;
        }
        let root = Self::home_dir();
        let Some(session_dir) = path.parent() else {
            return false;
        };
        session_dir.parent() == Some(root.as_path()) && Self::is_vendor_session_dir(session_dir)
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let candidate = Self::latest_session_dir_for_id(session_id)?.join(MESSAGES_FILENAME);
        debug!(
            provider = "vibe",
            path = %candidate.display(),
            session_id,
            "owns session"
        );
        Some(candidate)
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Vibe session");

        let file = std::fs::File::open(path)
            .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", path.display()))?;
        let reader = std::io::BufReader::new(file);

        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }

            let val: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let role_str = Self::extract_role(&val);
            // Vibe's session loader discards persisted system rows before it
            // constructs the resumable conversation. Mirror that visibility
            // here so a conversion never treats them as resumable history.
            if role_str == "system" {
                continue;
            }
            let role = normalize_role(&role_str);
            let content = Self::extract_content(&val);

            if content.trim().is_empty() {
                continue;
            }

            let ts = Self::extract_timestamp(&val);
            if started_at.is_none() {
                started_at = ts;
            }
            if ts.is_some() {
                ended_at = ts;
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content,
                timestamp: ts,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: val,
            });
        }

        reindex_messages(&mut messages);

        // Vibe's directory ends in only the first eight id characters.  The
        // metadata carries the complete id that `vibe --resume` accepts.
        let metadata = path.parent().and_then(Self::session_metadata);
        let session_id = metadata
            .as_ref()
            .and_then(Self::metadata_session_id)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                    })
                    .to_string()
            });

        let title = metadata
            .as_ref()
            .and_then(|metadata| metadata.get("title"))
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });
        let workspace = metadata
            .as_ref()
            .and_then(|metadata| metadata.get("environment"))
            .and_then(|environment| environment.get("working_directory"))
            .and_then(Value::as_str)
            .filter(|workspace| !workspace.is_empty())
            .map(PathBuf::from);

        let metadata = serde_json::json!({ "source": "vibe" });

        info!(session_id, messages = messages.len(), "Vibe session parsed");

        Ok(CanonicalSession {
            session_id,
            provider_slug: "vibe".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata,
            source_path: path.to_path_buf(),
            model_name: None,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let source_session_id = if session.session_id.is_empty() {
            format!("ags-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"))
        } else {
            session.session_id.clone()
        };
        let session_id = Self::native_session_id(&source_session_id);

        // `SessionLogger.save_folder` writes `session_<UTC>_<id[:8]>`.  The
        // full id lives in metadata and is what `vibe --resume` resolves.
        let short_id = &session_id[..8];
        let candidates = Self::vendor_candidates(short_id).map_err(|error| {
            crate::error::AgsError::SessionWriteError {
                path: Self::home_dir(),
                provider: self.slug().to_string(),
                detail: format!(
                    "failed to inspect Vibe's `{short_id}` short-id candidates: {error}"
                ),
            }
        })?;
        if let Some((collision_dir, collision_id, _)) = candidates
            .iter()
            .find(|(_, full_id, _)| full_id.as_deref() != Some(session_id.as_str()))
        {
            let collision_id = collision_id.as_deref().unwrap_or("<missing>");
            return Err(crate::error::AgsError::SessionWriteError {
                path: collision_dir.clone(),
                provider: self.slug().to_string(),
                detail: format!(
                    "Vibe short-id collision: `{short_id}` also resolves session `{collision_id}`; refusing to write because `vibe --resume` could open the wrong conversation"
                ),
            }
            .into());
        }
        let existing_dir = candidates
            .into_iter()
            .max_by_key(|(_, _, modified)| *modified)
            .map(|(session_dir, _, _)| session_dir);
        let target_dir = match existing_dir {
            Some(existing_dir) if !opts.force => {
                return Err(crate::error::AgsError::SessionConflict {
                    session_id,
                    existing_path: existing_dir.join(MESSAGES_FILENAME),
                }
                .into());
            }
            Some(existing_dir) => existing_dir,
            None => {
                let generated = Self::home_dir().join(format!(
                    "{DEFAULT_SESSION_PREFIX}_{}_{}",
                    chrono::Utc::now().format("%Y%m%d_%H%M%S"),
                    short_id
                ));
                // A corrupt/racing entry can appear after the candidate scan.
                // `--force` must never turn that ambiguity into permission to
                // overwrite a directory we did not identify as this session.
                if generated.exists() {
                    return Err(crate::error::AgsError::SessionWriteError {
                        path: generated,
                        provider: self.slug().to_string(),
                        detail: "refusing to overwrite an unrecognized Vibe session directory"
                            .to_string(),
                    }
                    .into());
                }
                generated
            }
        };
        let target_path = target_dir.join(MESSAGES_FILENAME);
        let metadata_path = target_dir.join(METADATA_FILENAME);

        debug!(
            session_id,
            path = %target_path.display(),
            messages = session.messages.len(),
            "writing Vibe session"
        );

        let mut lines: Vec<String> = Vec::with_capacity(session.messages.len());
        for msg in &session.messages {
            let role_str = match &msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                // Vibe discards persisted `system` rows on resume and has no
                // role for arbitrary source labels.  Keep the words as user
                // turns; `pipeline::folded_role` reports the role loss.
                MessageRole::System | MessageRole::Other(_) => "user",
                MessageRole::Tool => "tool",
            };

            let mut obj = serde_json::Map::new();
            obj.insert(
                "role".into(),
                serde_json::Value::String(role_str.to_string()),
            );
            obj.insert(
                "content".into(),
                serde_json::Value::String(msg.content.clone()),
            );
            if let Some(ts) = msg.timestamp {
                let dt =
                    chrono::DateTime::from_timestamp_millis(ts).unwrap_or_else(chrono::Utc::now);
                obj.insert(
                    "timestamp".into(),
                    serde_json::Value::String(dt.to_rfc3339()),
                );
            }

            lines.push(serde_json::to_string(&serde_json::Value::Object(obj))?);
        }

        let content = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        };
        let messages_outcome = crate::pipeline::atomic_write(
            &target_path,
            content.as_bytes(),
            opts.force,
            self.slug(),
        )?;

        let start_time = session
            .started_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
        let end_time = session
            .ended_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|timestamp| timestamp.to_rfc3339());
        let workspace = session
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.to_str())
            .map(str::to_owned);
        let metadata = serde_json::json!({
            "session_id": &session_id,
            "parent_session_id": null,
            "start_time": start_time,
            "end_time": end_time,
            "git_commit": null,
            "git_branch": null,
            "environment": { "working_directory": workspace },
            "username": "ags",
            "loops": [],
            "title": session.title,
            "title_source": "auto",
            "experiments": null,
            "total_messages": lines.len(),
        });
        let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
        let metadata_outcome = match crate::pipeline::atomic_write(
            &metadata_path,
            &metadata_bytes,
            opts.force,
            self.slug(),
        ) {
            Ok(outcome) => outcome,
            Err(write_error) => {
                if let Err(rollback_error) =
                    crate::pipeline::restore_backup(&messages_outcome, self.slug())
                {
                    return Err(anyhow::anyhow!(
                        "failed to write Vibe metadata ({write_error}); transcript rollback also failed ({rollback_error})"
                    ));
                }
                return Err(write_error.into());
            }
        };

        let mut backups = Vec::new();
        backups.extend(messages_outcome.displaced());
        backups.extend(metadata_outcome.displaced());
        let resume_command = self.resume_command(&session_id);
        let warnings = if session
            .workspace
            .as_ref()
            .is_none_or(|workspace| !workspace.is_absolute())
        {
            vec![format!(
                "Vibe's session picker filters by the current working directory, but this session has no absolute workspace and may not appear there; resume it directly with `{resume_command}`"
            )]
        } else {
            Vec::new()
        };

        info!(
            session_id,
            path = %messages_outcome.target_path.display(),
            messages = session.messages.len(),
            "Vibe session written"
        );

        Ok(WrittenSession {
            // The pipeline reads back `paths[0]`; it must remain the
            // transcript rather than the metadata sidecar.
            paths: vec![
                messages_outcome.target_path.clone(),
                metadata_outcome.target_path.clone(),
            ],
            session_id: session_id.clone(),
            resume_command,
            backups,
            warnings,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("vibe --resume {session_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn write_vibe_session(dir: &Path, session_id: &str, lines: &[&str]) -> PathBuf {
        let session_dir = dir.join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("messages.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn read_vibe(session_id: &str, lines: &[&str]) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_vibe_session(tmp.path(), session_id, lines);
        let provider = Vibe;
        provider.read_session(&path).expect("read_session failed")
    }

    // -----------------------------------------------------------------------
    // Reader tests
    // -----------------------------------------------------------------------

    #[test]
    fn reader_basic_exchange() {
        let session = read_vibe(
            "sess-1",
            &[
                r#"{"role":"user","content":"Hello","timestamp":"2025-01-27T03:30:00.000Z"}"#,
                r#"{"role":"assistant","content":"Hi!","timestamp":"2025-01-27T03:30:05.000Z"}"#,
            ],
        );

        assert_eq!(session.provider_slug, "vibe");
        assert_eq!(session.session_id, "sess-1");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn reader_flexible_role_field() {
        // Test "speaker" as role field name.
        let session = read_vibe(
            "sess-2",
            &[
                r#"{"speaker":"user","content":"Hello"}"#,
                r#"{"speaker":"assistant","content":"Hi!"}"#,
            ],
        );
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn reader_nested_message_role() {
        let session = read_vibe(
            "sess-3",
            &[
                r#"{"message":{"role":"user","content":"Hello"}}"#,
                r#"{"message":{"role":"assistant","content":"Hi!"}}"#,
            ],
        );
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello");
    }

    #[test]
    fn reader_text_field_as_content() {
        let session = read_vibe(
            "sess-4",
            &[r#"{"role":"user","text":"Hello via text field"}"#],
        );
        assert_eq!(session.messages[0].content, "Hello via text field");
    }

    #[test]
    fn reader_flexible_timestamp_fields() {
        let session = read_vibe(
            "sess-5",
            &[
                r#"{"role":"user","content":"A","created_at":"2025-01-27T03:30:00.000Z"}"#,
                r#"{"role":"user","content":"B","createdAt":"2025-01-27T03:31:00.000Z"}"#,
                r#"{"role":"user","content":"C","time":"2025-01-27T03:32:00.000Z"}"#,
                r#"{"role":"user","content":"D","ts":"2025-01-27T03:33:00.000Z"}"#,
            ],
        );
        assert_eq!(session.messages.len(), 4);
        assert!(session.messages[0].timestamp.is_some());
        assert!(session.messages[1].timestamp.is_some());
        assert!(session.messages[2].timestamp.is_some());
        assert!(session.messages[3].timestamp.is_some());
    }

    #[test]
    fn reader_skips_empty_content() {
        let session = read_vibe(
            "sess-6",
            &[
                r#"{"role":"user","content":"Valid"}"#,
                r#"{"role":"assistant","content":""}"#,
                r#"{"role":"assistant","content":"  "}"#,
            ],
        );
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn reader_skips_system_rows_the_vendor_drops_on_resume() {
        let session = read_vibe(
            "sess-system",
            &[
                r#"{"role":"system","content":"Never reaches a resumed Vibe conversation"}"#,
                r#"{"role":"user","content":"Visible prompt"}"#,
            ],
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Visible prompt");
    }

    #[test]
    fn reader_skips_invalid_json() {
        let session = read_vibe(
            "sess-7",
            &["", "not-json", r#"{"role":"user","content":"Valid"}"#],
        );
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn reader_session_id_from_parent_dir() {
        let session = read_vibe("my-session-abc", &[r#"{"role":"user","content":"test"}"#]);
        assert_eq!(session.session_id, "my-session-abc");
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_vibe(
            "sess-8",
            &[
                r#"{"role":"assistant","content":"Welcome"}"#,
                r#"{"role":"user","content":"Refactor the auth module"}"#,
            ],
        );
        assert_eq!(session.title.as_deref(), Some("Refactor the auth module"));
    }

    #[test]
    fn reader_empty_file() {
        let session = read_vibe("empty", &[]);
        assert_eq!(session.messages.len(), 0);
        assert!(session.title.is_none());
    }

    #[test]
    fn reader_metadata_has_source() {
        let session = read_vibe("sess-9", &[r#"{"role":"user","content":"test"}"#]);
        assert_eq!(session.metadata["source"], "vibe");
    }

    #[test]
    fn reader_reindexes_messages() {
        let session = read_vibe(
            "sess-10",
            &[
                r#"{"role":"user","content":"A"}"#,
                r#"{"role":"assistant","content":"B"}"#,
                r#"{"role":"user","content":"C"}"#,
            ],
        );
        assert_eq!(session.messages[0].idx, 0);
        assert_eq!(session.messages[1].idx, 1);
        assert_eq!(session.messages[2].idx, 2);
    }

    // -----------------------------------------------------------------------
    // Writer tests
    // -----------------------------------------------------------------------

    #[test]
    fn writer_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("rt-test");
        std::fs::create_dir_all(&session_dir).unwrap();

        let original = CanonicalSession {
            session_id: "rt-test".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: None,
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
                    content: "Done.".to_string(),
                    timestamp: Some(1_700_000_500_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
            ],
            metadata: json!({}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: None,
        };

        // Write directly to the session dir.
        let target = session_dir.join("messages.jsonl");
        let mut lines = Vec::new();
        for msg in &original.messages {
            let role_str = match &msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => "other",
            };
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), json!(role_str));
            obj.insert("content".into(), json!(&msg.content));
            lines.push(serde_json::to_string(&serde_json::Value::Object(obj)).unwrap());
        }
        std::fs::write(&target, lines.join("\n") + "\n").unwrap();

        let provider = Vibe;
        let readback = provider.read_session(&target).unwrap();
        assert_eq!(readback.messages.len(), 2);
        assert_eq!(readback.messages[0].content, "Fix the bug");
        assert_eq!(readback.messages[1].content, "Done.");
    }

    #[test]
    fn writer_resume_command() {
        let provider = Vibe;
        assert_eq!(
            provider.resume_command("my-session"),
            "vibe --resume my-session"
        );
    }

    // -----------------------------------------------------------------------
    // Provider metadata
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let provider = Vibe;
        assert_eq!(provider.name(), "Vibe");
        assert_eq!(provider.slug(), "vibe");
        assert_eq!(provider.cli_alias(), "vib");
    }
}
