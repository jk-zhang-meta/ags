//! Aider provider — reads Markdown chat history sessions.
//!
//! Session files: `.aider.chat.history.md` (per-project, in the git repo root)
//! Resume command: `aider --chat-history-file <path> --restore-chat-history`
//!
//! ## Markdown format
//!
//! Aider uses an append-only Markdown file with three content types:
//!
//! - `# aider chat started at YYYY-MM-DD HH:MM:SS` — session boundary header
//! - `#### <user text>` — user messages (H4 headings)
//! - `> <tool output>` — tool/system output (blockquotes)
//! - Everything else — assistant responses (bare text)
//!
//! ## Session ID scheme
//!
//! Aider has no native session IDs. casr derives a deterministic ID from the
//! session start timestamp: `YYYY-MM-DDThh-mm-ss`. Histories written by casr
//! carry an ignored metadata comment with a unique `ags-<uuid>` ID.
//!
//! ## Multi-session files
//!
//! A single `.aider.chat.history.md` may contain many sessions (append-only).
//! casr uses a virtual path scheme `<history-file>/<session-id>` (like Cursor)
//! to address individual sessions within a multi-session file.
//!
//! ## Where the history file lives
//!
//! Aider does not keep a central session store; it keeps one history file per
//! *repository*, and it resolves the path itself. Verified against the
//! aider 0.86.2 sdist:
//!
//! - `aider/args.py:274-287` — the default for `--chat-history-file` is
//!   `os.path.join(git_root, ".aider.chat.history.md") if git_root else
//!   ".aider.chat.history.md"`.
//! - `aider/main.py:462` / `:60-66` — `git_root` is
//!   `git.Repo(search_parent_directories=True).working_tree_dir`, i.e. the
//!   nearest enclosing git work tree, **not** the process working directory.
//! - `aider/args.py:41` — the parser is built with
//!   `auto_env_var_prefix="AIDER_"`, which is what makes
//!   `AIDER_CHAT_HISTORY_FILE` an alias for `--chat-history-file`.
//!
//! [`Aider::find_history_files`] reproduces that rule instead of approximating
//! it, so running casr from anywhere inside a repository finds the same file
//! aider would append to.
//!
//! ## Writing
//!
//! casr never appends to Aider's shared history. Each conversion gets a
//! dedicated `.aider.chat.history.ags-<uuid>.md`, and the launch specification
//! passes that exact path through `--chat-history-file` together with
//! `--restore-chat-history`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, trace};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession, read_dir_reporting,
    walk_entry_reporting,
};

/// Aider provider implementation.
pub struct Aider;

/// The fixed basename aider gives its Markdown chat history
/// (`aider/args.py:274-287`).
const HISTORY_FILE_NAME: &str = ".aider.chat.history.md";
const GENERATED_HISTORY_PREFIX: &str = ".aider.chat.history.ags-";
const SESSION_ID_PREFIX: &str = "# ags session id: ";
const WORKSPACE_PREFIX: &str = "# ags workspace: ";
const MESSAGE_BOUNDARY: &str = "ags message boundary";

/// Represents a single parsed session within an Aider history file.
struct ParsedSession {
    /// Deterministic session ID from the start timestamp.
    session_id: String,
    /// The raw start timestamp string from the header.
    start_timestamp: String,
    /// The full text of just this session (from header to next header or EOF).
    text: String,
}

impl Aider {
    /// Tree casr scans for history files, from `AIDER_HOME`.
    ///
    /// `AIDER_HOME` is **casr's own** override (the README's "casr's own
    /// override" column), not one of aider's: the aider 0.86.2 sdist contains
    /// no occurrence of the name, and aider has no `--home` argument for its
    /// `auto_env_var_prefix="AIDER_"` parser to derive it from. It aims casr at
    /// a tree of checkouts without touching aider. Aider's *own* variable,
    /// `AIDER_CHAT_HISTORY_FILE`, is honoured separately in
    /// [`Self::find_history_files`].
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("AIDER_HOME") {
            return Some(PathBuf::from(home));
        }
        None
    }

    /// Find every `.aider.chat.history.md` casr can account for.
    ///
    /// Steps 2 and 3 reproduce aider's own resolution of the history path (see
    /// the module docs for the exact sdist references): the file lives at the
    /// *git work-tree root*, found by walking parents, and falls back to the
    /// working directory only when there is no repository at all.
    fn find_history_files() -> Vec<PathBuf> {
        let mut unreadable = Vec::new();
        Self::history_files_from(
            std::env::current_dir().ok().as_deref(),
            &mut unreadable,
        )
    }

    fn is_history_file_name(name: &str) -> bool {
        name == HISTORY_FILE_NAME
            || (name.starts_with(GENERATED_HISTORY_PREFIX) && name.ends_with(".md"))
    }

    fn add_history_files_in(
        dir: &Path,
        files: &mut Vec<PathBuf>,
        unreadable: &mut Vec<UnreadableSource>,
    ) {
        for entry in read_dir_reporting(dir, unreadable) {
            let path = entry.path();
            let is_history = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(Self::is_history_file_name);
            if path.is_file() && is_history && !files.contains(&path) {
                files.push(path);
            }
        }
    }

    /// [`Self::find_history_files`] with the working directory injected, so the
    /// git-root walk is testable without mutating process-global state.
    fn history_files_from(
        cwd: Option<&Path>,
        unreadable: &mut Vec<UnreadableSource>,
    ) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();

        // 1. casr's own override: scan the tree it points at.
        if let Some(home) = Self::home_dir() {
            Self::scan_for_history_files(&home, &mut files, unreadable, 4);
        }

        // 2. Aider's own override, which names the file outright.
        if let Ok(path) = std::env::var("AIDER_CHAT_HISTORY_FILE") {
            let p = PathBuf::from(path);
            if p.is_file() && !files.contains(&p) {
                files.push(p);
            }
        }

        // 3. Aider's default: the enclosing git work-tree root, else the CWD.
        //    Checking the CWD alone found sessions only when the shell happened
        //    to sit exactly at the repository root.
        if let Some(cwd) = cwd {
            let git_root = crate::discovery::find_git_root(cwd);
            for dir in git_root.as_deref().into_iter().chain(std::iter::once(cwd)) {
                Self::add_history_files_in(dir, &mut files, unreadable);
            }
        }

        files
    }

    /// Walk a directory for `.aider.chat.history.md` files.
    fn scan_for_history_files(
        dir: &Path,
        files: &mut Vec<PathBuf>,
        unreadable: &mut Vec<UnreadableSource>,
        max_depth: usize,
    ) {
        match std::fs::metadata(dir) {
            Ok(metadata) if !metadata.is_dir() => {
                unreadable.push(UnreadableSource {
                    path: dir.to_path_buf(),
                    error: "expected a directory, found a non-directory path".to_string(),
                });
                return;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                unreadable.push(UnreadableSource {
                    path: dir.to_path_buf(),
                    error: error.to_string(),
                });
                return;
            }
        }
        for entry in WalkDir::new(dir).max_depth(max_depth) {
            let Some(entry) = walk_entry_reporting(entry, unreadable) else {
                continue;
            };
            if entry
                .file_name()
                .to_str()
                .is_some_and(Self::is_history_file_name)
                && entry.path().is_file()
                && !files.contains(&entry.path().to_path_buf())
            {
                files.push(entry.path().to_path_buf());
            }
        }
    }

    /// Build a virtual per-session path within a history file.
    ///
    /// Format: `<history_file_path>/<session_id>`
    fn virtual_session_path(history_path: &Path, session_id: &str) -> PathBuf {
        let encoded = urlencoding::encode(session_id);
        history_path.join(encoded.as_ref())
    }

    /// Extract the history file path and session ID from a virtual path.
    ///
    /// Returns `(history_file_path, session_id)`.
    fn parse_virtual_path(path: &Path) -> Option<(PathBuf, String)> {
        let parent = path.parent()?;
        let filename = path.file_name()?.to_str()?;

        // If the parent is an Aider history file, this is a virtual path.
        if parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(Self::is_history_file_name)
        {
            let decoded = urlencoding::decode(filename).ok()?;
            return Some((parent.to_path_buf(), decoded.into_owned()));
        }

        None
    }

    /// Split a history file into individual session texts.
    fn split_sessions(content: &str) -> Vec<ParsedSession> {
        let mut sessions: Vec<ParsedSession> = Vec::new();
        let mut current_text = String::new();
        let mut current_timestamp = String::new();
        let mut current_id = String::new();

        for line in content.lines() {
            if let Some(ts) = parse_session_header(line) {
                // Flush previous session.
                if !current_id.is_empty() && !current_text.trim().is_empty() {
                    sessions.push(ParsedSession {
                        session_id: current_id.clone(),
                        start_timestamp: current_timestamp.clone(),
                        text: std::mem::take(&mut current_text),
                    });
                }
                current_timestamp = ts.clone();
                current_id = timestamp_to_session_id(&ts);
                current_text = format!("{line}\n");
            } else if let Some(session_id) = line.strip_prefix(SESSION_ID_PREFIX)
                && !current_id.is_empty()
                && !session_id.trim().is_empty()
            {
                current_id = session_id.trim().to_string();
                current_text.push_str(line);
                current_text.push('\n');
            } else {
                current_text.push_str(line);
                current_text.push('\n');
            }
        }

        // Flush last session.
        if !current_id.is_empty() && !current_text.trim().is_empty() {
            sessions.push(ParsedSession {
                session_id: current_id,
                start_timestamp: current_timestamp,
                text: current_text,
            });
        }

        sessions
    }

    /// Parse a single session text block into a `CanonicalSession`.
    fn parse_session_text(
        path: &Path,
        session: &ParsedSession,
    ) -> anyhow::Result<CanonicalSession> {
        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut user_lines: Vec<String> = Vec::new();
        let mut assistant_lines: Vec<String> = Vec::new();
        let mut tool_lines: Vec<String> = Vec::new();
        let mut model_name: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;

        // Flush accumulated lines into a message.
        let flush_user = |lines: &mut Vec<String>, msgs: &mut Vec<CanonicalMessage>| {
            if lines.is_empty() {
                return;
            }
            let content = lines.join("\n").trim().to_string();
            lines.clear();
            if content.is_empty() || content == "<blank>" {
                return;
            }
            msgs.push(CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content,
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            });
        };

        let flush_assistant = |lines: &mut Vec<String>, msgs: &mut Vec<CanonicalMessage>| {
            if lines.is_empty() {
                return;
            }
            let content = lines.join("\n").trim().to_string();
            lines.clear();
            if content.is_empty() {
                return;
            }
            msgs.push(CanonicalMessage {
                idx: 0,
                role: MessageRole::Assistant,
                content,
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            });
        };

        let flush_tool = |lines: &mut Vec<String>, msgs: &mut Vec<CanonicalMessage>| {
            if lines.is_empty() {
                return;
            }
            let content = lines.join("\n").trim().to_string();
            lines.clear();
            if content.is_empty() {
                return;
            }
            msgs.push(CanonicalMessage {
                idx: 0,
                role: MessageRole::Tool,
                content,
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            });
        };

        for line in session.text.lines() {
            if let Some(encoded) = line.strip_prefix(WORKSPACE_PREFIX) {
                if workspace.is_none()
                    && let Ok(value) = serde_json::from_str::<String>(encoded)
                {
                    workspace = Some(PathBuf::from(value));
                }
                continue;
            }

            // Skip the session header.
            if line.starts_with("# ") {
                continue;
            }

            // Tool/system output: lines starting with "> ".
            if let Some(rest) = line.strip_prefix("> ") {
                // Flush other accumulators first.
                flush_assistant(&mut assistant_lines, &mut messages);
                flush_user(&mut user_lines, &mut messages);

                let stripped = rest.trim_end().trim_end_matches("  ");
                if stripped == MESSAGE_BOUNDARY {
                    flush_tool(&mut tool_lines, &mut messages);
                    continue;
                }
                let parsed_model = extract_model_from_tool_line(stripped);
                let parsed_workspace = extract_workspace_from_tool_line(stripped);
                let is_metadata_only_line = parsed_model.is_some() || parsed_workspace.is_some();

                // Extract metadata from tool output lines.
                if model_name.is_none()
                    && let Some(model) = parsed_model
                {
                    model_name = Some(model);
                }
                if workspace.is_none()
                    && let Some(ws) = parsed_workspace
                {
                    workspace = Some(ws);
                }
                if !is_metadata_only_line {
                    tool_lines.push(stripped.to_string());
                }

                continue;
            }

            // User message: lines starting with "#### ".
            if let Some(rest) = line.strip_prefix("#### ") {
                flush_assistant(&mut assistant_lines, &mut messages);
                flush_tool(&mut tool_lines, &mut messages);

                let stripped = rest.trim_end().trim_end_matches("  ");
                user_lines.push(stripped.to_string());
                continue;
            }

            // Everything else is assistant text.
            flush_user(&mut user_lines, &mut messages);
            flush_tool(&mut tool_lines, &mut messages);

            assistant_lines.push(line.to_string());
        }

        // Flush remaining lines.
        flush_user(&mut user_lines, &mut messages);
        flush_assistant(&mut assistant_lines, &mut messages);
        flush_tool(&mut tool_lines, &mut messages);

        reindex_messages(&mut messages);

        // Parse start timestamp into epoch millis.
        let started_at = parse_aider_timestamp(&session.start_timestamp);
        let ended_at = started_at; // Aider doesn't have per-message timestamps.

        // Title from first user message.
        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        // If workspace not found in tool output, try to derive from file path.
        if workspace.is_none() {
            workspace = path.parent().map(PathBuf::from);
        }

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("aider".to_string()),
        );
        metadata.insert(
            "start_timestamp_raw".into(),
            serde_json::Value::String(session.start_timestamp.clone()),
        );

        let source_path = Self::virtual_session_path(path, &session.session_id);

        debug!(
            session_id = session.session_id,
            messages = messages.len(),
            "Aider session parsed"
        );

        Ok(CanonicalSession {
            session_id: session.session_id.clone(),
            provider_slug: "aider".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path,
            model_name,
        })
    }

    fn write_root() -> anyhow::Result<PathBuf> {
        if let Some(home) = Self::home_dir() {
            return Ok(home);
        }
        if let Ok(history) = std::env::var("AIDER_CHAT_HISTORY_FILE")
            && !history.trim().is_empty()
        {
            return PathBuf::from(history)
                .parent()
                .map(Path::to_path_buf)
                .context("AIDER_CHAT_HISTORY_FILE has no parent directory");
        }

        let cwd = std::env::current_dir().context("could not determine Aider write directory")?;
        Ok(crate::discovery::find_git_root(&cwd).unwrap_or(cwd))
    }

    fn message_text(message: &CanonicalMessage) -> String {
        let mut text = message.content.clone();
        let mut append = |block: String| {
            if text.contains(&block) {
                return;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block);
        };
        for call in &message.tool_calls {
            append(format!("[Tool: {}]", call.name));
        }
        for result in &message.tool_results {
            if !result.content.trim().is_empty() && message.content == result.content {
                continue;
            }
            append(if result.is_error {
                format!("[Tool Error] {}", result.content)
            } else {
                format!("[Tool Output] {}", result.content)
            });
        }
        text
    }

    fn render_history(
        session: &CanonicalSession,
        session_id: &str,
        workspace: &Path,
    ) -> anyhow::Result<String> {
        let started = session
            .started_at
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
            .map(|time| time.with_timezone(&chrono::Local))
            .unwrap_or_else(chrono::Local::now)
            .format("%Y-%m-%d %H:%M:%S");
        let workspace_json =
            serde_json::to_string(&workspace.display().to_string()).expect("string is valid JSON");
        let mut out = format!(
            "# aider chat started at {started}\n{SESSION_ID_PREFIX}{session_id}\n\
             {WORKSPACE_PREFIX}{workspace_json}\n"
        );
        let mut previous_user: Option<bool> = None;

        for (index, message) in session.messages.iter().enumerate() {
            let text = Self::message_text(message);
            if text.is_empty() {
                anyhow::bail!(
                    "Aider cannot restore empty message {index}; refusing to write a session that \
                     would change its message count"
                );
            }
            if text.trim() != text {
                anyhow::bail!(
                    "Aider cannot round-trip leading or trailing whitespace in message {index}; \
                     refusing instead of silently changing its content"
                );
            }

            let as_user = message.role != MessageRole::Assistant;
            if previous_user == Some(as_user) {
                // Aider's official splitter discards tool/block-quote messages
                // during restore. This line therefore flushes the previous
                // same-role message without adding a model-visible turn.
                out.push_str("> ");
                out.push_str(MESSAGE_BOUNDARY);
                out.push('\n');
            }

            if as_user {
                for line in text.split('\n') {
                    out.push_str("#### ");
                    out.push_str(line);
                    out.push('\n');
                }
            } else {
                if let Some(line) = text.lines().find(|line| {
                    line.starts_with("# ")
                        || line.starts_with("#### ")
                        || line.starts_with("> ")
                }) {
                    anyhow::bail!(
                        "Aider's official history parser treats assistant line {line:?} as \
                         metadata, user text, or tool output; refusing message {index} because no \
                         native lossless representation exists"
                    );
                }
                out.push_str(&text);
                out.push('\n');
            }
            previous_user = Some(as_user);
        }

        Ok(out)
    }

    fn resume_spec_for_path(
        session_id: &str,
        history_path: &Path,
        workspace: &Path,
    ) -> LaunchSpec {
        LaunchSpec::new(
            "aider",
            [
                "--chat-history-file".to_string(),
                history_path.display().to_string(),
                "--restore-chat-history".to_string(),
            ],
        )
        .in_dir(workspace)
        .targeting_session(session_id)
    }

    fn located_resume_spec(session_id: &str) -> Option<LaunchSpec> {
        let provider = Self;
        let locator = provider.owns_session(session_id)?;
        let (history_path, _) = Self::parse_virtual_path(&locator)?;
        let workspace = provider
            .read_session(&locator)
            .ok()
            .and_then(|session| session.workspace)
            .filter(|path| path.is_dir())
            .or_else(|| history_path.parent().map(Path::to_path_buf))?;
        Some(Self::resume_spec_for_path(
            session_id,
            &history_path,
            &workspace,
        ))
    }
}

impl Provider for Aider {
    fn name(&self) -> &str {
        "Aider"
    }

    fn slug(&self) -> &str {
        "aider"
    }

    fn cli_alias(&self) -> &str {
        "aid"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("aider").is_ok() {
            evidence.push("aider binary found in PATH".to_string());
            installed = true;
        }

        let history_files = Self::find_history_files();
        if !history_files.is_empty() {
            evidence.push(format!(
                "{} .aider.chat.history.md file(s) found",
                history_files.len()
            ));
            installed = true;
        }

        trace!(provider = "aider", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        // Return parent directories of all known history files.
        let mut roots: Vec<PathBuf> = Vec::new();
        for file in Self::find_history_files() {
            if let Some(parent) = file.parent() {
                let parent_buf = parent.to_path_buf();
                if !roots.contains(&parent_buf) {
                    roots.push(parent_buf);
                }
            }
        }
        roots
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        for history_file in Self::find_history_files() {
            let Ok(content) = std::fs::read_to_string(&history_file) else {
                continue;
            };
            if Self::split_sessions(&content)
                .iter()
                .any(|session| session.session_id == session_id)
            {
                let virtual_path = Self::virtual_session_path(&history_file, session_id);
                debug!(
                    history_file = %history_file.display(),
                    session_path = %virtual_path.display(),
                    session_id,
                    "found Aider session"
                );
                return Some(virtual_path);
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Aider session");

        // Check if this is a virtual path (history_file/session_id).
        if let Some((history_path, session_id)) = Self::parse_virtual_path(path) {
            let content = std::fs::read_to_string(&history_path)
                .with_context(|| format!("failed to read {}", history_path.display()))?;
            let sessions = Self::split_sessions(&content);
            let session = sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "session {} not found in {}",
                        session_id,
                        history_path.display()
                    )
                })?;
            return Self::parse_session_text(&history_path, session);
        }

        // Direct file path — read the whole file and return the last session.
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let sessions = Self::split_sessions(&content);

        if sessions.is_empty() {
            // Treat the entire file as a single session.
            let session = ParsedSession {
                session_id: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
                start_timestamp: String::new(),
                text: content,
            };
            return Self::parse_session_text(path, &session);
        }

        // Return the last (most recent) session.
        let last = sessions.last().expect("checked non-empty");
        Self::parse_session_text(path, last)
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let root = Self::write_root()?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create Aider history directory {}", root.display()))?;
        let session_id = format!("ags-{}", uuid::Uuid::new_v4().simple());
        let target_path = root.join(format!(".aider.chat.history.{session_id}.md"));
        let workspace = session
            .workspace
            .as_deref()
            .filter(|path| path.is_dir())
            .unwrap_or(&root);
        let content = Self::render_history(session, &session_id, workspace)?;
        let outcome = crate::pipeline::atomic_write(
            &target_path,
            content.as_bytes(),
            opts.force,
            self.slug(),
        )?;
        let spec = Self::resume_spec_for_path(&session_id, &outcome.target_path, workspace);
        let warnings = session.workspace.as_ref().map_or_else(Vec::new, |source| {
            if source.is_dir() {
                Vec::new()
            } else {
                vec![format!(
                    "The source workspace {} does not exist; Aider will start in {}.",
                    source.display(),
                    workspace.display()
                )]
            }
        });

        debug!(
            session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "Aider session written through an independent native history file"
        );
        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id,
            resume_command: spec.display(),
            backups: outcome.displaced().into_iter().collect(),
            warnings,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        Self::located_resume_spec(session_id).map_or_else(
            || "aider --restore-chat-history".to_string(),
            |spec| spec.display(),
        )
    }

    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        Self::located_resume_spec(session_id).or_else(|| {
            Some(LaunchSpec::new(
                "aider",
                ["--restore-chat-history".to_string()],
            ))
        })
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let mut listing = SessionListing::default();
        let history_files = Self::history_files_from(
            std::env::current_dir().ok().as_deref(),
            &mut listing.unreadable,
        );
        for history_file in &history_files {
            // `find_history_files` only returns paths that exist, so a failure
            // to open one is a real refusal and not an absent store.
            let content = match std::fs::read_to_string(history_file) {
                Ok(content) => content,
                Err(error) => {
                    listing.cannot_read(history_file, &error);
                    continue;
                }
            };
            for session in Self::split_sessions(&content) {
                let virtual_path =
                    Self::virtual_session_path(history_file, &session.session_id);
                listing.sessions.push((session.session_id, virtual_path));
            }
        }

        Some(listing)
    }

    /// Aider appends every session to one Markdown file, so a "session path"
    /// here is the virtual `<history file>#<session id>` this provider mints,
    /// never a real file of its own.
    fn is_session_path(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(Self::is_history_file_name)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a session header line and extract the timestamp.
///
/// Expected format: `# aider chat started at YYYY-MM-DD HH:MM:SS`
/// Returns the timestamp portion (e.g. `"2024-08-05 19:33:02"`).
fn parse_session_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    trimmed
        .strip_prefix("# aider chat started at ")
        .map(|ts| ts.trim().to_string())
}

/// Convert a timestamp string to a deterministic session ID.
///
/// `"2024-08-05 19:33:02"` → `"2024-08-05T19-33-02"`
fn timestamp_to_session_id(timestamp: &str) -> String {
    timestamp.replace(' ', "T").replace(':', "-")
}

/// Parse an Aider timestamp string into epoch milliseconds.
///
/// Expected format: `YYYY-MM-DD HH:MM:SS`
fn parse_aider_timestamp(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    if ts.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp_millis())
}

/// Extract model name from an Aider tool output line.
///
/// Looks for patterns like:
/// - `"Models: claude-3-5-sonnet-20240620 with diff edit format"`
/// - `"Model: gpt-4o-mini with whole edit format"`
fn extract_model_from_tool_line(line: &str) -> Option<String> {
    let line = line.trim();
    for prefix in ["Models: ", "Model: "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            // Take up to " with " to get just the model name.
            let model = rest.split(" with ").next().unwrap_or(rest).trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    None
}

/// Extract workspace path from an Aider tool output line.
///
/// Looks for patterns like:
/// - `"Git repo: .git with 300 files"` → derive from the history file path
/// - Absolute path references in tool output
fn extract_workspace_from_tool_line(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    // Look for absolute paths.
    for prefix in ["/data/projects/", "/home/", "/Users/", "/root/"] {
        if let Some(idx) = line.find(prefix) {
            let rest = &line[idx..];
            let path: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .collect();
            if path.len() > prefix.len() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    // -----------------------------------------------------------------------
    // Session header parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_session_header_standard() {
        assert_eq!(
            parse_session_header("# aider chat started at 2024-08-05 19:33:02"),
            Some("2024-08-05 19:33:02".to_string())
        );
    }

    #[test]
    fn parse_session_header_with_whitespace() {
        assert_eq!(
            parse_session_header("  # aider chat started at 2024-08-05 19:33:02  "),
            Some("2024-08-05 19:33:02".to_string())
        );
    }

    #[test]
    fn parse_session_header_not_a_header() {
        assert_eq!(parse_session_header("#### User message"), None);
        assert_eq!(parse_session_header("> tool output"), None);
        assert_eq!(parse_session_header("assistant text"), None);
    }

    // -----------------------------------------------------------------------
    // Timestamp to session ID
    // -----------------------------------------------------------------------

    #[test]
    fn timestamp_to_session_id_standard() {
        assert_eq!(
            timestamp_to_session_id("2024-08-05 19:33:02"),
            "2024-08-05T19-33-02"
        );
    }

    // -----------------------------------------------------------------------
    // Aider timestamp parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_aider_timestamp_standard() {
        let result = parse_aider_timestamp("2024-08-05 19:33:02");
        assert!(result.is_some());
        assert!(result.unwrap() > 1_700_000_000_000);
    }

    #[test]
    fn parse_aider_timestamp_empty() {
        assert_eq!(parse_aider_timestamp(""), None);
    }

    #[test]
    fn parse_aider_timestamp_garbage() {
        assert_eq!(parse_aider_timestamp("not a date"), None);
    }

    // -----------------------------------------------------------------------
    // Model extraction
    // -----------------------------------------------------------------------

    #[test]
    fn extract_model_standard() {
        assert_eq!(
            extract_model_from_tool_line(
                "Models: claude-3-5-sonnet-20240620 with diff edit format, weak model claude-3-haiku"
            ),
            Some("claude-3-5-sonnet-20240620".to_string())
        );
    }

    #[test]
    fn extract_model_single_model() {
        assert_eq!(
            extract_model_from_tool_line("Model: gpt-4o-mini with whole edit format"),
            Some("gpt-4o-mini".to_string())
        );
    }

    #[test]
    fn extract_model_no_model() {
        assert_eq!(
            extract_model_from_tool_line("Git repo: .git with 300 files"),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Session splitting
    // -----------------------------------------------------------------------

    #[test]
    fn split_sessions_single() {
        let content = "\
# aider chat started at 2024-08-05 19:33:02

> Aider v0.47.2-dev

#### Hello

Hi there!

";
        let sessions = Aider::split_sessions(content);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "2024-08-05T19-33-02");
        assert_eq!(sessions[0].start_timestamp, "2024-08-05 19:33:02");
    }

    #[test]
    fn split_sessions_multiple() {
        let content = "\
# aider chat started at 2024-08-05 19:33:02

#### First session

Response one

# aider chat started at 2024-08-05 20:45:10

#### Second session

Response two

";
        let sessions = Aider::split_sessions(content);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "2024-08-05T19-33-02");
        assert_eq!(sessions[1].session_id, "2024-08-05T20-45-10");
    }

    #[test]
    fn split_sessions_empty() {
        let sessions = Aider::split_sessions("");
        assert_eq!(sessions.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Reader unit tests
    // -----------------------------------------------------------------------

    /// Write content to a temp file and read it back.
    fn read_aider_session(content: &str) -> CanonicalSession {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".md").unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let aider = Aider;
        aider
            .read_session(tmp.path())
            .unwrap_or_else(|e| panic!("read_session failed: {e}"))
    }

    #[test]
    fn reader_basic_exchange() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

> Aider v0.47.2-dev
> Models: claude-3-5-sonnet with diff edit format

#### Fix the bug in main.rs

I'll fix the bug. Here's the change:

```python
print('fixed')
```

> Applied edit to main.rs
",
        );
        assert_eq!(session.session_id, "2024-08-05T19-33-02");
        assert_eq!(session.provider_slug, "aider");
        // Should have: tool (Aider banner), user, assistant, tool (applied edit)
        assert!(session.messages.len() >= 2);

        // Check we have user and assistant messages.
        let user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        let asst_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "Fix the bug in main.rs");
        assert!(!asst_msgs.is_empty());
        assert!(asst_msgs[0].content.contains("I'll fix the bug"));

        // Model extracted from tool output.
        assert_eq!(session.model_name.as_deref(), Some("claude-3-5-sonnet"));
    }

    #[test]
    fn reader_multi_line_user_input() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### First line of input
#### Second line of input
#### Third line

Response here.

",
        );
        let user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert!(user_msgs[0].content.contains("First line of input"));
        assert!(user_msgs[0].content.contains("Second line of input"));
        assert!(user_msgs[0].content.contains("Third line"));
    }

    #[test]
    fn reader_blank_user_input_skipped() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### <blank>

Some response

#### Real message

Another response

",
        );
        let user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "Real message");
    }

    #[test]
    fn reader_tool_output_as_separate_messages() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

> Aider v0.47.2-dev

#### Hello

Response

> Applied edit to file.rs
> Commit abc123 fix: something

",
        );
        let tool_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert!(!tool_msgs.is_empty());
    }

    #[test]
    fn reader_returns_last_session_from_multi_session_file() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### First session

First response

# aider chat started at 2024-08-05 20:45:10

#### Second session

Second response

",
        );
        assert_eq!(session.session_id, "2024-08-05T20-45-10");
        let user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "Second session");
    }

    #[test]
    fn reader_empty_file() {
        let session = read_aider_session("");
        assert_eq!(session.messages.len(), 0);
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### Refactor the authentication module

Done.

",
        );
        assert_eq!(
            session.title.as_deref(),
            Some("Refactor the authentication module")
        );
    }

    #[test]
    fn reader_preserves_code_blocks_in_assistant() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### Fix the function

Here's the fix:

```rust
fn main() {
    println!(\"hello\");
}
```

",
        );
        let asst_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .collect();
        assert!(asst_msgs[0].content.contains("```rust"));
        assert!(asst_msgs[0].content.contains("fn main()"));
    }

    #[test]
    fn reader_slash_commands_as_user_messages() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### /diff

> Some diff output

#### /ex

",
        );
        let user_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert!(!user_msgs.is_empty());
        assert_eq!(user_msgs[0].content, "/diff");
    }

    #[test]
    fn reader_started_at_timestamp() {
        let session = read_aider_session(
            "\
# aider chat started at 2024-08-05 19:33:02

#### Hello

Hi!

",
        );
        assert!(session.started_at.is_some());
    }

    // -----------------------------------------------------------------------
    // Virtual path tests
    // -----------------------------------------------------------------------

    #[test]
    fn virtual_path_round_trip() {
        let history = Path::new("/data/project/.aider.chat.history.md");
        let session_id = "2024-08-05T19-33-02";
        let virtual_path = Aider::virtual_session_path(history, session_id);

        let (parsed_path, parsed_id) =
            Aider::parse_virtual_path(&virtual_path).expect("should parse virtual path");
        assert_eq!(parsed_path, history);
        assert_eq!(parsed_id, session_id);
    }

    // -----------------------------------------------------------------------
    // History-file discovery — must match aider's own path rule
    // -----------------------------------------------------------------------

    /// Build `<tmp>/repo` as a git work tree holding a history file, plus a
    /// nested `<tmp>/repo/src/deep` to run "from". Returns both paths.
    fn repo_with_history(tmp: &Path) -> (PathBuf, PathBuf) {
        let repo = tmp.join("repo");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::create_dir(repo.join(".git")).expect("create .git dir");
        let history = repo.join(HISTORY_FILE_NAME);
        std::fs::write(
            &history,
            "# aider chat started at 2024-08-05 19:33:02\n\n#### hello  \n",
        )
        .expect("write history file");
        (nested, history)
    }

    #[test]
    fn history_file_found_from_a_subdirectory_of_the_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (nested, history) = repo_with_history(tmp.path());

        // aider writes at the git work-tree root and finds it from anywhere
        // inside the repo (`aider/main.py:462` → `search_parent_directories`).
        // casr must resolve the same file, not just the one in the CWD.
        let found = Aider::history_files_from(Some(&nested), &mut Vec::new());
        assert!(
            found.contains(&history),
            "expected {} to be discovered from {}, got {found:?}",
            history.display(),
            nested.display()
        );
    }

    #[test]
    fn history_file_found_at_the_git_root_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let (nested, history) = repo_with_history(tmp.path());
        let repo = nested.parent().unwrap().parent().unwrap();

        let found = Aider::history_files_from(Some(repo), &mut Vec::new());
        assert!(found.contains(&history), "got {found:?}");
        // The git-root and CWD candidates are the same path here — it must be
        // reported once, not twice.
        assert_eq!(
            found.iter().filter(|p| **p == history).count(),
            1,
            "history file must not be duplicated: {found:?}"
        );
    }

    #[test]
    fn history_file_found_outside_any_repository() {
        // With no git root, aider defaults to `./.aider.chat.history.md`
        // (`aider/args.py:274-287`), so the CWD candidate must survive.
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("no-repo");
        std::fs::create_dir(&plain).expect("create dir");
        let history = plain.join(HISTORY_FILE_NAME);
        std::fs::write(&history, "# aider chat started at 2024-08-05 19:33:02\n")
            .expect("write history file");

        let found = Aider::history_files_from(Some(&plain), &mut Vec::new());
        assert!(found.contains(&history), "got {found:?}");
    }

    #[test]
    fn no_history_file_means_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).expect("create dir");

        let found = Aider::history_files_from(Some(&empty), &mut Vec::new());
        assert!(
            !found.iter().any(|p| p.starts_with(&empty)),
            "must not invent a path that does not exist: {found:?}"
        );
    }

    #[test]
    fn history_scan_reports_a_non_directory_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("not-a-directory");
        std::fs::write(&root, b"not a history tree").expect("write root file");
        let mut files = Vec::new();
        let mut unreadable = Vec::new();

        Aider::scan_for_history_files(&root, &mut files, &mut unreadable, 4);

        assert!(files.is_empty());
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].path, root);
    }

    // -----------------------------------------------------------------------
    // Provider trait tests
    // -----------------------------------------------------------------------

    #[test]
    fn resume_command_uses_restore_flag() {
        let provider = Aider;
        assert_eq!(
            <Aider as Provider>::resume_command(&provider, "any-id"),
            "aider --restore-chat-history"
        );
    }

    #[test]
    fn provider_metadata() {
        let provider = Aider;
        assert_eq!(provider.name(), "Aider");
        assert_eq!(provider.slug(), "aider");
        assert_eq!(provider.cli_alias(), "aid");
    }

    // -----------------------------------------------------------------------
    // Writer helper tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // list_sessions
    // -----------------------------------------------------------------------

    #[test]
    fn list_sessions_enumerates_all_sessions_in_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let history_path = tmp_dir.path().join(".aider.chat.history.md");
        std::fs::write(
            &history_path,
            "\
# aider chat started at 2024-08-05 19:33:02

#### First session

Response one

# aider chat started at 2024-08-05 20:45:10

#### Second session

Response two

# aider chat started at 2024-08-06 10:00:00

#### Third session

Response three

",
        )
        .unwrap();

        // split_sessions should find all 3 sessions
        let content = std::fs::read_to_string(&history_path).unwrap();
        let sessions = Aider::split_sessions(&content);
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].session_id, "2024-08-05T19-33-02");
        assert_eq!(sessions[1].session_id, "2024-08-05T20-45-10");
        assert_eq!(sessions[2].session_id, "2024-08-06T10-00-00");
    }

    #[test]
    fn independent_history_round_trips_without_touching_the_shared_history() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let session = CanonicalSession {
            session_id: "test-123".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(tmp_dir.path().to_path_buf()),
            title: Some("Test".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_001_000_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Fix the bug".to_string(),
                    timestamp: None,
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "I'll fix it now.".to_string(),
                    timestamp: None,
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
            ],
            metadata: json!({"source": "claude-code"}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: Some("claude-3".to_string()),
        };

        let session_id = "ags-test-session";
        let history = Aider::render_history(&session, session_id, tmp_dir.path())
            .expect("render independent history");
        let path = tmp_dir
            .path()
            .join(format!(".aider.chat.history.{session_id}.md"));
        std::fs::write(&path, history).expect("write isolated history");
        let readback = Aider.read_session(&path).expect("read rendered history");

        assert_eq!(readback.session_id, session_id);
        assert_eq!(readback.messages.len(), 2);
        assert_eq!(readback.messages[0].content, "Fix the bug");
        assert_eq!(readback.messages[1].content, "I'll fix it now.");
        assert!(
            !tmp_dir.path().join(HISTORY_FILE_NAME).exists(),
            "the independent writer must not append to Aider's shared history"
        );
    }
}
