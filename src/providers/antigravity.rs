//! Antigravity CLI (`agy`) provider — reads conversations under
//! `~/.gemini/antigravity-cli/`.
//!
//! `agy` is Google's Antigravity CLI, the successor to the retired Gemini CLI
//! (`gmi`). The two tools **share** the `~/.gemini` parent directory, so this
//! provider is carefully disambiguated from [`crate::providers::gemini::Gemini`]:
//!
//! - `~/.gemini/tmp/<hash>/chats/session-*.json` → **gmi** (legacy Gemini CLI)
//! - `~/.gemini/antigravity-cli/conversations/<uuid>.db` → **agy** (Antigravity)
//!
//! ## On-disk layout
//!
//! - Conversation databases: `~/.gemini/antigravity-cli/conversations/<uuid>.db`
//!   — stock SQLite (`SQLite format 3`, `user_version 1`). The `<uuid>`
//!   (filename stem) is the conversation id passed to `agy --conversation
//!   <uuid>`. The DB stores the trajectory as protobuf blobs (`steps`,
//!   `trajectory_meta`, …), which are opaque, so it is used only to enumerate
//!   conversations and (as a fallback) derive a title.
//! - Clean transcript (preferred source): `~/.gemini/antigravity-cli/brain/
//!   <uuid>/.system_generated/logs/transcript.jsonl` — one JSON object per
//!   step with `step_index`, `source`, `type`, `status`, `created_at`,
//!   `content`, and optional `thinking` / `tool_calls`.
//!
//! ## Transcript step model
//!
//! | `source`         | role               |
//! |------------------|--------------------|
//! | `USER_EXPLICIT`  | [`MessageRole::User`]      |
//! | `MODEL`          | [`MessageRole::Assistant`] |
//! | `SYSTEM`         | [`MessageRole::System`]    |
//!
//! User content is wrapped in `<USER_REQUEST>…</USER_REQUEST>` tags which are
//! unwrapped for the canonical title/content. `SYSTEM` housekeeping steps
//! (`CONVERSATION_HISTORY`, `EPHEMERAL_MESSAGE`) carry no useful conversation
//! content and are skipped.
//!
//! ## Resume mechanism
//!
//! `agy --conversation <uuid>` — the conversation id and nothing else.
//!
//! ### Why no model is pinned
//!
//! ags used to emit `--model "Gemini 3.1 Pro (High)"` alongside it. Verified
//! against the shipped `agy` 1.1.7 linux-x64 build (`antigravity --help`, run
//! under a throwaway `HOME`), that flag surface is:
//!
//! ```text
//!   --conversation  Resume a previous conversation by ID
//!   --model         Model for the current CLI session
//!   --effort        Reasoning effort for the current CLI session (low|medium|high)
//! ```
//!
//! So model and reasoning effort are *two* flags, and `--model` takes the
//! "stable, user-facing model slugs that appear in the `/model` picker"
//! introduced in agy 1.1.5 — lowercase forms such as `gemini-3.1-pro`, which is
//! what the binary's own string table contains. No parenthesized
//! `"<Model> (<Effort>)"` label exists anywhere in it; that shape is a desktop
//! picker caption, not a CLI value. The old command could therefore never
//! resolve: since agy 1.1.2 an unresolvable `--model` hard-fails print mode
//! with a non-zero exit, and in an interactive session silently falls back to
//! another model with a warning.
//!
//! Emitting a *correct* pin would be no better. ags has nothing to derive one
//! from: the transcript records no model (see [`Antigravity::read_session`]),
//! so any slug ags chose would be ags's opinion, silently overriding whatever
//! the user picked. agy already persists the user's own `/model` and `/effort`
//! choice across sessions, so omitting both flags resumes on the model the user
//! actually selected. The argument the other way — that pinning makes a resumed
//! conversation reproducible across machines, and stops a cheap default model
//! from inheriting an expensive conversation — is real, but it is a decision
//! for the user's agy settings or their own command line, not for a resume
//! command ags prints on their behalf. If a caller wants a pin, appending
//! `--model <slug> --effort <low|medium|high>` is theirs to do.
//!
//! ## Write support
//!
//! Antigravity's official Python SDK can create the same SQLite trajectory that
//! `agy` resumes. The writer uses `google-antigravity>=0.1.9` with a loopback
//! OpenAI-compatible endpoint: source user turns are submitted to the official
//! harness and the endpoint returns the corresponding source assistant turns.
//! The SDK then reopens the conversation and verifies every stored user/model
//! step before the database is moved into the CLI's conversation store.
//!
//! The public SDK has no assistant/system/tool injection API. Adjacent
//! non-assistant messages are therefore visibly labelled and coalesced into one
//! user turn, and adjacent assistant messages are coalesced into one model turn.
//! A trailing user-side turn is refused because inventing an assistant reply
//! would make the resumed history claim that an unanswered request was answered.
//! The generated transcript sidecar preserves the canonical preview and makes
//! ags's normal read-back verification and rollback cover the import.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, parse_timestamp, reindex_messages,
    truncate_title,
};
use crate::providers::{
    Provider, SessionListing, WriteOptions, WrittenSession, read_dir_reporting, store_evidence,
};

/// Antigravity CLI provider implementation.
pub struct Antigravity;

const ANTIGRAVITY_SDK_REQUIRED: &str = "Antigravity target writes require Python and the official \
`google-antigravity>=0.1.9` SDK. Install it with \
`python3 -m pip install \"google-antigravity>=0.1.9\"`, or point \
`AGS_ANTIGRAVITY_PYTHON` at a Python interpreter where it is installed.";

const ANTIGRAVITY_PYTHON_ENV: &str = "AGS_ANTIGRAVITY_PYTHON";

const SDK_PROBE: &str = r#"
import importlib.metadata
import sys
from google.antigravity import Agent, LocalOpenAIAgentConfig, types

version = importlib.metadata.version("google-antigravity").split("+", 1)[0]
parts = tuple(int(part) for part in version.split(".")[:3])
if parts < (0, 1, 9):
  raise SystemExit(2)
if not hasattr(types.BuiltinTools, "none"):
  raise SystemExit(3)
print(sys.executable)
"#;

const SDK_BRIDGE: &str = r#"
import asyncio
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import threading
import time

from google.antigravity import Agent, LocalOpenAIAgentConfig, types


class ReplayHandler(BaseHTTPRequestHandler):
  protocol_version = "HTTP/1.1"
  responses = deque()
  path_token = ""

  def log_message(self, format, *args):
    return

  def do_GET(self):
    if self.path.startswith(self.path_token) and self.path.rstrip("/").endswith("/models"):
      self.send_json({
          "object": "list",
          "data": [{
              "id": "ags-import",
              "object": "model",
              "created": 0,
              "owned_by": "ags",
          }],
      })
      return
    self.send_error(404)

  def do_POST(self):
    if (
        not self.path.startswith(self.path_token)
        or not self.path.rstrip("/").endswith("/chat/completions")
    ):
      self.send_error(404)
      return

    length = int(self.headers.get("Content-Length", "0"))
    payload = json.loads(self.rfile.read(length) or b"{}")
    if not self.responses:
      self.send_error(500, "unexpected extra model request")
      return
    content = self.responses.popleft()

    if payload.get("stream"):
      self.send_response(200)
      self.send_header("Content-Type", "text/event-stream")
      self.send_header("Cache-Control", "no-cache")
      self.send_header("Connection", "close")
      self.end_headers()
      chunks = [
          {
              "id": "chatcmpl-ags",
              "object": "chat.completion.chunk",
              "created": int(time.time()),
              "model": "ags-import",
              "choices": [{
                  "index": 0,
                  "delta": {"role": "assistant", "content": content},
                  "finish_reason": None,
              }],
          },
          {
              "id": "chatcmpl-ags",
              "object": "chat.completion.chunk",
              "created": int(time.time()),
              "model": "ags-import",
              "choices": [{
                  "index": 0,
                  "delta": {},
                  "finish_reason": "stop",
              }],
          },
      ]
      for chunk in chunks:
        self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
      self.wfile.write(b"data: [DONE]\n\n")
      self.wfile.flush()
      return

    self.send_json({
        "id": "chatcmpl-ags",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": "ags-import",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2,
        },
    })

  def send_json(self, payload):
    encoded = json.dumps(payload).encode()
    self.send_response(200)
    self.send_header("Content-Type", "application/json")
    self.send_header("Content-Length", str(len(encoded)))
    self.end_headers()
    self.wfile.write(encoded)


def make_config(port, token, save_dir, app_data_dir, conversation_id=None):
  return LocalOpenAIAgentConfig(
      model="ags-import",
      base_url=f"http://127.0.0.1:{port}/{token}/v1",
      capabilities=types.CapabilitiesConfig(
          enabled_tools=types.BuiltinTools.none(),
          enable_subagents=False,
      ),
      policies=[],
      workspaces=[],
      conversation_id=conversation_id,
      save_dir=str(save_dir),
      app_data_dir=str(app_data_dir),
  )


async def import_and_verify(turns, save_dir, app_data_dir, port, token):
  conversation_id = None
  async with Agent(make_config(port, token, save_dir, app_data_dir)) as agent:
    for index, turn in enumerate(turns):
      response = await agent.chat(turn["user"])
      actual = await response.text()
      if actual != turn["assistant"]:
        raise RuntimeError(
            f"assistant replay mismatch at turn {index}: "
            f"expected {len(turn['assistant'])} chars, got {len(actual)}"
        )
    conversation_id = agent.conversation_id

  if not conversation_id:
    raise RuntimeError("official SDK returned no conversation ID")
  conversation_id = str(conversation_id)
  if any(
      not (character.isascii() and (character.isalnum() or character == "-"))
      for character in conversation_id
  ):
    raise RuntimeError("official SDK returned an invalid conversation ID")

  expected = []
  for turn in turns:
    expected.extend([
        {"source": "USER", "content": turn["user"]},
        {"source": "MODEL", "content": turn["assistant"]},
    ])

  async with Agent(
      make_config(port, token, save_dir, app_data_dir, conversation_id)
  ) as resumed:
    actual = [
        {"source": step.source.value, "content": step.content}
        for step in resumed.conversation.history
        if step.source.value in ("USER", "MODEL")
    ]
    if actual != expected:
      raise RuntimeError(
          f"official SDK resume verification changed the projected history: "
          f"expected {len(expected)} steps, got {len(actual)}"
      )

  return conversation_id


def main():
  request = json.load(sys.stdin)
  cli_dir = Path(request["cli_dir"]).resolve()
  conversations_dir = Path(request["conversations_dir"]).resolve()
  turns = request["turns"]
  if not turns:
    raise ValueError("at least one replay turn is required")
  for index, turn in enumerate(turns):
    if not isinstance(turn.get("user"), str) or not turn["user"]:
      raise ValueError(f"turn {index} has no user content")
    if not isinstance(turn.get("assistant"), str) or not turn["assistant"]:
      raise ValueError(f"turn {index} has no assistant content")

  cli_dir.mkdir(parents=True, exist_ok=True)
  conversations_dir.mkdir(parents=True, exist_ok=True)
  token = secrets.token_urlsafe(24)
  ReplayHandler.responses = deque(turn["assistant"] for turn in turns)
  ReplayHandler.path_token = f"/{token}/"
  server = ThreadingHTTPServer(("127.0.0.1", 0), ReplayHandler)
  thread = threading.Thread(target=server.serve_forever, daemon=True)
  thread.start()

  target = None
  committed = False
  try:
    with tempfile.TemporaryDirectory(
        prefix=".ags-antigravity-", dir=cli_dir
    ) as staging:
      staging = Path(staging)
      save_dir = staging / "conversations"
      app_data_dir = staging / "app-data"
      save_dir.mkdir()
      app_data_dir.mkdir()
      try:
        conversation_id = asyncio.run(
            import_and_verify(
                turns,
                save_dir,
                app_data_dir,
                server.server_address[1],
                token,
            )
        )
      finally:
        server.shutdown()
        thread.join()
        server.server_close()

      staged_db = save_dir / f"{conversation_id}.db"
      if not staged_db.is_file():
        raise RuntimeError("official SDK did not create the expected conversation database")
      if staged_db.read_bytes()[:16] != b"SQLite format 3\x00":
        raise RuntimeError("official SDK output is not a SQLite conversation database")

      target = conversations_dir / f"{conversation_id}.db"
      if target.exists():
        raise FileExistsError(f"target conversation already exists: {target}")
      os.rename(staged_db, target)
      result = {"conversation_id": conversation_id}
      print(json.dumps(result), flush=True)
      committed = True
  finally:
    if server.fileno() != -1:
      server.shutdown()
      thread.join()
      server.server_close()
    if target is not None and not committed:
      try:
        target.unlink()
      except FileNotFoundError:
        pass


if __name__ == "__main__":
  main()
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReplayTurn {
    user: String,
    assistant: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayProjection {
    turns: Vec<ReplayTurn>,
    coalesced_user_messages: usize,
    coalesced_assistant_messages: usize,
}

#[derive(Serialize)]
struct BridgeRequest<'a> {
    cli_dir: &'a Path,
    conversations_dir: &'a Path,
    turns: &'a [ReplayTurn],
}

#[derive(Deserialize)]
struct BridgeResponse {
    // Paths are derived in Rust: Python resolves symlinks before writing, so
    // comparing the two path spellings would reject the same target.
    conversation_id: String,
}

impl Antigravity {
    /// Root directory for the shared Gemini family data.
    /// Respects the `GEMINI_HOME` env var override (shared with the legacy
    /// Gemini CLI provider so a single override relocates both).
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("GEMINI_HOME") {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir().map(|h| h.join(".gemini"))
    }

    /// The Antigravity CLI data directory: `<home>/antigravity-cli`.
    fn cli_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("antigravity-cli"))
    }

    /// The conversations directory holding `<uuid>.db` files.
    fn conversations_dir() -> Option<PathBuf> {
        Self::cli_dir().map(|d| d.join("conversations"))
    }

    /// Path to the clean transcript JSONL for a conversation uuid.
    fn transcript_path(cli_dir: &Path, uuid: &str) -> PathBuf {
        cli_dir
            .join("brain")
            .join(uuid)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl")
    }

    /// Path to the conversation database file for a uuid.
    fn db_path(conversations_dir: &Path, uuid: &str) -> PathBuf {
        conversations_dir.join(format!("{uuid}.db"))
    }

    /// Enumerate `(uuid, db_path)` for every conversation database under the
    /// configured conversations directory.
    fn list_conversations() -> SessionListing {
        let Some(conv_dir) = Self::conversations_dir() else {
            return SessionListing::default();
        };
        list_conversations_in(&conv_dir)
    }
}

fn python_with_sdk() -> Option<PathBuf> {
    let configured = std::env::var_os(ANTIGRAVITY_PYTHON_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let candidates: Vec<PathBuf> = match configured {
        Some(path) => vec![path],
        None => ["python3", "python"]
            .into_iter()
            .filter_map(|name| which::which(name).ok())
            .collect(),
    };

    candidates.into_iter().find(|python| {
        Command::new(python)
            .args(["-c", SDK_PROBE])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn role_label(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "assistant".to_string(),
        MessageRole::Tool => "tool".to_string(),
        MessageRole::System => "system".to_string(),
        MessageRole::Other(name) => format!("other:{name}"),
    }
}

fn render_user_batch(messages: &[&CanonicalMessage]) -> String {
    if let [message] = messages
        && message.role == MessageRole::User
    {
        return message.content.clone();
    }

    messages
        .iter()
        .map(|message| {
            format!(
                "[ags imported {} message]\n{}",
                role_label(&message.role),
                message.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn project_replay(session: &CanonicalSession) -> anyhow::Result<ReplayProjection> {
    let mut turns: Vec<ReplayTurn> = Vec::new();
    let mut pending_user_messages: Vec<&CanonicalMessage> = Vec::new();
    let mut coalesced_user_messages = 0usize;
    let mut coalesced_assistant_messages = 0usize;

    for message in &session.messages {
        if message.role != MessageRole::Assistant {
            pending_user_messages.push(message);
            continue;
        }

        if message.content.is_empty() {
            anyhow::bail!(
                "Antigravity import cannot replay empty assistant message {} through the official SDK",
                message.idx
            );
        }

        if pending_user_messages.is_empty() {
            let Some(previous) = turns.last_mut() else {
                anyhow::bail!(
                    "Antigravity import cannot start with an assistant message: the official SDK \
                     creates model output only in response to a user-side turn"
                );
            };
            previous
                .assistant
                .push_str("\n\n[ags imported assistant continuation]\n");
            previous.assistant.push_str(&message.content);
            coalesced_assistant_messages += 1;
            continue;
        }

        if pending_user_messages
            .iter()
            .any(|pending| pending.content.is_empty())
        {
            anyhow::bail!(
                "Antigravity import cannot replay an empty user-side message through the official SDK"
            );
        }

        coalesced_user_messages += pending_user_messages.len().saturating_sub(1);
        turns.push(ReplayTurn {
            user: render_user_batch(&pending_user_messages),
            assistant: message.content.clone(),
        });
        pending_user_messages.clear();
    }

    if !pending_user_messages.is_empty() {
        anyhow::bail!(
            "Antigravity import cannot preserve {} trailing user-side message(s) without inventing \
             an assistant reply; resume the source agent once to complete that turn, then convert again",
            pending_user_messages.len()
        );
    }
    if turns.is_empty() {
        anyhow::bail!("Antigravity import has no complete user/assistant turn to replay");
    }

    Ok(ReplayProjection {
        turns,
        coalesced_user_messages,
        coalesced_assistant_messages,
    })
}

fn run_sdk_bridge(
    python: &Path,
    cli_dir: &Path,
    conversations_dir: &Path,
    projection: &ReplayProjection,
) -> anyhow::Result<BridgeResponse> {
    let request = BridgeRequest {
        cli_dir,
        conversations_dir,
        turns: &projection.turns,
    };
    let mut child = Command::new(python)
        .args(["-c", SDK_BRIDGE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start Antigravity SDK bridge with {}",
                python.display()
            )
        })?;

    let write_result = child
        .stdin
        .take()
        .context("Antigravity SDK bridge stdin was not available")
        .and_then(|mut stdin| {
            serde_json::to_writer(&mut stdin, &request)
                .context("failed to send session to Antigravity SDK bridge")?;
            stdin
                .flush()
                .context("failed to flush Antigravity SDK bridge input")
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let output = child
        .wait_with_output()
        .context("failed while waiting for Antigravity SDK bridge")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        anyhow::bail!(
            "official Antigravity SDK import failed{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }

    serde_json::from_slice(&output.stdout)
        .context("official Antigravity SDK bridge returned invalid JSON")
}

fn transcript_source(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User | MessageRole::Other(_) => "USER_EXPLICIT",
        MessageRole::Assistant => "MODEL",
        MessageRole::Tool => "TOOL",
        MessageRole::System => "SYSTEM",
    }
}

fn transcript_type(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User | MessageRole::Other(_) => "USER_INPUT",
        MessageRole::Assistant => "PLANNER_RESPONSE",
        MessageRole::Tool => "TOOL_RESULT",
        MessageRole::System => "SYSTEM_MESSAGE",
    }
}

fn render_import_transcript(session: &CanonicalSession) -> anyhow::Result<Vec<u8>> {
    let mut transcript = Vec::new();
    for message in &session.messages {
        let content = if message.role == MessageRole::User {
            format!("<USER_REQUEST>\n{}\n</USER_REQUEST>", message.content)
        } else {
            message.content.clone()
        };
        let mut step = serde_json::json!({
            "step_index": message.idx,
            "source": transcript_source(&message.role),
            "type": transcript_type(&message.role),
            "status": "DONE",
            "content": content,
            "tool_calls": message.tool_calls,
            "ags_imported": true,
            "ags_original_role": role_label(&message.role),
        });
        if let Some(timestamp) = message.timestamp {
            step["created_at"] = serde_json::Value::Number(timestamp.into());
        }
        serde_json::to_writer(&mut transcript, &step)
            .context("failed to serialize Antigravity import transcript")?;
        transcript.push(b'\n');
    }
    Ok(transcript)
}

/// Enumerate `(uuid, db_path)` for every `<uuid>.db` directly under `conv_dir`.
///
/// The uuid is the filename stem. Non-`.db` files (and the sibling legacy gmi
/// `tmp/.../chats/session-*.json` layout, which never lives here) are ignored,
/// which is what keeps the agy provider disjoint from the Gemini CLI provider.
fn list_conversations_in(conv_dir: &Path) -> SessionListing {
    let mut listing = SessionListing::default();
    for entry in read_dir_reporting(conv_dir, &mut listing.unreadable) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only `*.db` files; the uuid is the filename stem.
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        let Some(uuid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid.is_empty() {
            continue;
        }
        listing.sessions.push((uuid.to_string(), path));
    }
    listing
}

impl Provider for Antigravity {
    fn name(&self) -> &str {
        "Antigravity CLI"
    }

    fn slug(&self) -> &str {
        "antigravity"
    }

    fn cli_alias(&self) -> &str {
        "agy"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("agy").is_ok() {
            evidence.push("agy binary found in PATH".to_string());
            installed = true;
        }
        if let Some(python) = python_with_sdk() {
            evidence.push(format!(
                "official google-antigravity>=0.1.9 SDK found via {}",
                python.display()
            ));
        } else {
            evidence.push(
                "target writes unavailable: official google-antigravity>=0.1.9 SDK not found"
                    .to_string(),
            );
        }

        if let Some(cli_dir) = Self::cli_dir()
            && cli_dir.is_dir()
        {
            evidence.push(format!("{} exists", cli_dir.display()));
            installed = true;
        }

        // `list` reads `<cli dir>/conversations`, not the CLI directory that
        // detection above accepted.
        if installed && let Some(conversations) = Self::conversations_dir() {
            evidence.push(store_evidence(&conversations));
        }

        trace!(provider = "antigravity", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let Some(conv_dir) = Self::conversations_dir() else {
            return vec![];
        };
        if conv_dir.is_dir() {
            vec![conv_dir]
        } else {
            vec![]
        }
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        Some(Self::list_conversations())
    }

    /// One `<conversation-uuid>.db` per conversation, directly under the
    /// conversations directory. The `.db` extension is what keeps this provider
    /// disjoint from the Gemini CLI's `session-*.json` layout.
    fn is_session_path(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("db")
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let conv_dir = Self::conversations_dir()?;
        if !conv_dir.is_dir() {
            return None;
        }

        // The session id is the conversation uuid == filename stem.
        let candidate = Self::db_path(&conv_dir, session_id);
        if candidate.is_file() {
            debug!(path = %candidate.display(), session_id, "found Antigravity conversation");
            return Some(candidate);
        }

        // Case-insensitive fallback (UUIDs are conventionally lowercase, but be
        // robust to user-typed mixed case).
        let lc = session_id.to_ascii_lowercase();
        for (uuid, path) in Self::list_conversations().sessions {
            if uuid.to_ascii_lowercase() == lc {
                debug!(path = %path.display(), session_id, "found Antigravity conversation (case-insensitive)");
                return Some(path);
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Antigravity conversation");

        // `path` is the `<uuid>.db` file. The uuid is the filename stem.
        let uuid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_string());

        // The CLI dir is the grandparent of the db (conversations/<uuid>.db).
        let cli_dir = path
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(Self::cli_dir)
            .ok_or_else(|| anyhow::anyhow!("cannot determine Antigravity CLI directory"))?;

        let transcript = Self::transcript_path(&cli_dir, &uuid);

        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        if transcript.is_file() {
            let content = std::fs::read_to_string(&transcript)
                .with_context(|| format!("failed to read transcript {}", transcript.display()))?;

            for (i, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(step): Result<serde_json::Value, _> = serde_json::from_str(line) else {
                    trace!(line = i, "skipping malformed Antigravity transcript line");
                    continue;
                };

                let Some(msg) = step_to_message(&step) else {
                    continue;
                };

                if let Some(ts) = msg.timestamp {
                    started_at = Some(started_at.map_or(ts, |s: i64| s.min(ts)));
                    ended_at = Some(ended_at.map_or(ts, |e: i64| e.max(ts)));
                }

                messages.push(msg);
            }
        } else {
            debug!(
                transcript = %transcript.display(),
                "no transcript.jsonl found; Antigravity conversation has no readable preview"
            );
        }

        reindex_messages(&mut messages);

        // Title from the first user message (its content is already unwrapped).
        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("antigravity".to_string()),
        );
        metadata.insert(
            "conversation_uuid".into(),
            serde_json::Value::String(uuid.clone()),
        );
        metadata.insert(
            "transcript_path".into(),
            serde_json::Value::String(transcript.to_string_lossy().into_owned()),
        );

        debug!(
            session_id = uuid,
            messages = messages.len(),
            "Antigravity conversation parsed"
        );

        Ok(CanonicalSession {
            session_id: uuid,
            provider_slug: "antigravity".to_string(),
            workspace: None,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            // Unknown, and left unknown. Transcript steps carry no model
            // field, and the conversation `.db` keeps its trajectory as
            // opaque protobuf blobs, so there is nothing here to read a
            // model off. Naming one would be inventing it.
            model_name: None,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let python = python_with_sdk().ok_or_else(|| anyhow::anyhow!(ANTIGRAVITY_SDK_REQUIRED))?;
        let cli_dir = Self::cli_dir()
            .context("cannot determine Antigravity CLI directory for target write")?;
        let conversations_dir = cli_dir.join("conversations");
        let projection = project_replay(session)?;
        let response = run_sdk_bridge(&python, &cli_dir, &conversations_dir, &projection)?;

        if response.conversation_id.is_empty()
            || !response
                .conversation_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            anyhow::bail!("official Antigravity SDK returned an invalid conversation ID");
        }
        let db_path = Self::db_path(&conversations_dir, &response.conversation_id);
        let mut sqlite_header = [0u8; 16];
        if let Err(error) =
            std::fs::File::open(&db_path).and_then(|mut file| file.read_exact(&mut sqlite_header))
        {
            let rollback = std::fs::remove_file(&db_path);
            return Err(match rollback {
                Ok(()) => anyhow::anyhow!(
                    "failed to read Antigravity SDK output {}: {error}; rollback succeeded",
                    db_path.display()
                ),
                Err(rollback_error) => anyhow::anyhow!(
                    "failed to read Antigravity SDK output {}: {error}; rollback failed: \
                     {rollback_error}",
                    db_path.display()
                ),
            });
        }
        if sqlite_header != *b"SQLite format 3\0" {
            let rollback = std::fs::remove_file(&db_path);
            return Err(match rollback {
                Ok(()) => anyhow::anyhow!(
                    "official Antigravity SDK output {} is not a SQLite conversation; \
                     rollback succeeded",
                    db_path.display()
                ),
                Err(rollback_error) => anyhow::anyhow!(
                    "official Antigravity SDK output {} is not a SQLite conversation; \
                     rollback failed: {rollback_error}",
                    db_path.display()
                ),
            });
        }

        let transcript_path = Self::transcript_path(&cli_dir, &response.conversation_id);
        let transcript = match render_import_transcript(session) {
            Ok(transcript) => transcript,
            Err(error) => {
                let rollback = std::fs::remove_file(&db_path);
                return Err(match rollback {
                    Ok(()) => anyhow::anyhow!(
                        "failed to render Antigravity transcript after SDK import: {error:#}; \
                         rollback succeeded"
                    ),
                    Err(rollback_error) => anyhow::anyhow!(
                        "failed to render Antigravity transcript after SDK import: {error:#}; \
                         rollback failed: {rollback_error}"
                    ),
                });
            }
        };
        if let Err(error) =
            crate::pipeline::atomic_write(&transcript_path, &transcript, false, self.slug())
        {
            let rollback = std::fs::remove_file(&db_path);
            return Err(match rollback {
                Ok(()) => anyhow::anyhow!(
                    "failed to write Antigravity transcript after SDK import: {error}; rollback succeeded"
                ),
                Err(rollback_error) => anyhow::anyhow!(
                    "failed to write Antigravity transcript after SDK import: {error}; rollback failed: {rollback_error}"
                ),
            });
        }

        let mut warnings = vec![
            "Antigravity history was rebuilt by google-antigravity's official local harness; \
             source model reasoning and native tool trajectories are not available to replay."
                .to_string(),
        ];
        if projection.coalesced_user_messages > 0 {
            warnings.push(format!(
                "{} adjacent user/system/tool message(s) were visibly labelled and coalesced into \
                 neighbouring Antigravity user turns.",
                projection.coalesced_user_messages
            ));
        }
        if projection.coalesced_assistant_messages > 0 {
            warnings.push(format!(
                "{} adjacent assistant message(s) were visibly labelled and coalesced into \
                 neighbouring Antigravity model turns.",
                projection.coalesced_assistant_messages
            ));
        }

        Ok(WrittenSession {
            paths: vec![db_path, transcript_path],
            session_id: response.conversation_id.clone(),
            resume_command: self.resume_command(&response.conversation_id),
            backups: Vec::new(),
            warnings,
        })
    }

    fn write_refusal(&self) -> Option<&'static str> {
        python_with_sdk()
            .is_none()
            .then_some(ANTIGRAVITY_SDK_REQUIRED)
    }

    /// `agy --conversation <uuid>`, with no `--model` / `--effort` pin. See the
    /// module docs for why ags does not choose a model here.
    fn resume_command(&self, session_id: &str) -> String {
        format!("agy --conversation {session_id}")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a transcript step's `source` to a canonical role.
///
/// Returns `None` for sources we don't surface as conversation turns.
fn role_for_source(source: &str) -> Option<MessageRole> {
    match source {
        "USER_EXPLICIT" | "USER" => Some(MessageRole::User),
        "MODEL" => Some(MessageRole::Assistant),
        "TOOL" => Some(MessageRole::Tool),
        "SYSTEM" => Some(MessageRole::System),
        _ => None,
    }
}

/// `type` values that are pure housekeeping and carry no conversation content.
fn is_housekeeping_type(step_type: &str) -> bool {
    matches!(step_type, "CONVERSATION_HISTORY" | "EPHEMERAL_MESSAGE")
}

/// Unwrap the `<USER_REQUEST>…</USER_REQUEST>` envelope agy wraps user input in,
/// and strip the trailing `<ADDITIONAL_METADATA>` / `<USER_SETTINGS_CHANGE>`
/// system annotations. Returns the inner request text, trimmed.
fn unwrap_user_request(content: &str) -> String {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let after = &content[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim().to_string();
        }
    }
    // No envelope — but still drop any trailing metadata/settings annotations.
    let mut text = content;
    for marker in ["<ADDITIONAL_METADATA>", "<USER_SETTINGS_CHANGE>"] {
        if let Some(idx) = text.find(marker) {
            text = &text[..idx];
        }
    }
    text.trim().to_string()
}

/// Extract tool calls from a transcript step's `tool_calls` array, if present.
fn extract_tool_calls(step: &serde_json::Value) -> Vec<crate::model::ToolCall> {
    let Some(arr) = step.get("tool_calls").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|call| {
            let obj = call.as_object()?;
            Some(crate::model::ToolCall {
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
            })
        })
        .collect()
}

/// Convert a single transcript step into a [`CanonicalMessage`], or `None` if
/// the step is not a surfaceable conversation turn.
fn step_to_message(step: &serde_json::Value) -> Option<CanonicalMessage> {
    let source = step.get("source").and_then(|v| v.as_str()).unwrap_or("");
    let role = role_for_source(source)?;

    let step_type = step.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if is_housekeeping_type(step_type) {
        return None;
    }

    let raw_content = step.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let content = if role == MessageRole::User {
        unwrap_user_request(raw_content)
    } else {
        raw_content.trim().to_string()
    };

    // The model's internal reasoning is preserved as a fallback when the
    // visible content is empty (tool-only planner steps), mirroring the Gemini
    // provider's `thoughts` handling.
    let thinking = step
        .get("thinking")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let tool_calls = extract_tool_calls(step);

    let effective_content = if content.is_empty() && tool_calls.is_empty() {
        thinking.clone()
    } else {
        content
    };

    // Skip steps that carry no content and no tool activity at all.
    if effective_content.trim().is_empty() && tool_calls.is_empty() {
        return None;
    }

    let timestamp = step.get("created_at").and_then(parse_timestamp);

    Some(CanonicalMessage {
        idx: 0,
        role,
        content: effective_content,
        timestamp,
        author: None,
        tool_calls,
        tool_results: Vec::new(),
        extra: step.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ANTIGRAVITY_SDK_REQUIRED, Antigravity, is_housekeeping_type, list_conversations_in,
        project_replay, python_with_sdk, render_import_transcript, role_for_source,
        step_to_message, unwrap_user_request,
    };
    use crate::model::{CanonicalMessage, CanonicalSession, MessageRole};
    use crate::providers::Provider;
    use serde_json::json;
    use std::io::Write as _;

    // -----------------------------------------------------------------------
    // Role / type mapping
    // -----------------------------------------------------------------------

    #[test]
    fn role_for_source_maps_known_sources() {
        assert_eq!(role_for_source("USER_EXPLICIT"), Some(MessageRole::User));
        assert_eq!(role_for_source("USER"), Some(MessageRole::User));
        assert_eq!(role_for_source("MODEL"), Some(MessageRole::Assistant));
        assert_eq!(role_for_source("TOOL"), Some(MessageRole::Tool));
        assert_eq!(role_for_source("SYSTEM"), Some(MessageRole::System));
        assert_eq!(role_for_source("UNKNOWN_THING"), None);
    }

    #[test]
    fn housekeeping_types_recognized() {
        assert!(is_housekeeping_type("CONVERSATION_HISTORY"));
        assert!(is_housekeeping_type("EPHEMERAL_MESSAGE"));
        assert!(!is_housekeeping_type("USER_INPUT"));
        assert!(!is_housekeeping_type("PLANNER_RESPONSE"));
    }

    // -----------------------------------------------------------------------
    // User-request unwrapping
    // -----------------------------------------------------------------------

    #[test]
    fn unwrap_user_request_strips_envelope() {
        let raw = "<USER_REQUEST>\nFix the bug in main.rs\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-06-11T16:14:42-04:00.\n</ADDITIONAL_METADATA>";
        assert_eq!(unwrap_user_request(raw), "Fix the bug in main.rs");
    }

    #[test]
    fn unwrap_user_request_without_envelope_strips_metadata() {
        let raw = "Just do the thing\n<USER_SETTINGS_CHANGE>\nsomething\n</USER_SETTINGS_CHANGE>";
        assert_eq!(unwrap_user_request(raw), "Just do the thing");
    }

    #[test]
    fn unwrap_user_request_plain_text_passthrough() {
        assert_eq!(unwrap_user_request("plain text"), "plain text");
    }

    // -----------------------------------------------------------------------
    // step_to_message
    // -----------------------------------------------------------------------

    #[test]
    fn step_to_message_user_input() {
        let step = json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-06-11T20:14:42Z",
            "content": "<USER_REQUEST>\nHello agy\n</USER_REQUEST>"
        });
        let msg = step_to_message(&step).expect("user step should map");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "Hello agy");
        assert!(msg.timestamp.is_some());
    }

    #[test]
    fn step_to_message_model_response() {
        let step = json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "content": "Here is the summary of what I did."
        });
        let msg = step_to_message(&step).expect("model step should map");
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content, "Here is the summary of what I did.");
    }

    #[test]
    fn step_to_message_skips_housekeeping() {
        let history = json!({"source": "SYSTEM", "type": "CONVERSATION_HISTORY"});
        let ephemeral =
            json!({"source": "SYSTEM", "type": "EPHEMERAL_MESSAGE", "content": "noise"});
        assert!(step_to_message(&history).is_none());
        assert!(step_to_message(&ephemeral).is_none());
    }

    #[test]
    fn step_to_message_skips_empty_content() {
        let step = json!({"source": "MODEL", "type": "PLANNER_RESPONSE", "content": "   "});
        assert!(step_to_message(&step).is_none());
    }

    #[test]
    fn step_to_message_falls_back_to_thinking_for_tool_only_planner() {
        let step = json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "content": "",
            "thinking": "I will read the file first."
        });
        let msg = step_to_message(&step).expect("thinking fallback should map");
        assert_eq!(msg.content, "I will read the file first.");
        assert_eq!(msg.role, MessageRole::Assistant);
    }

    #[test]
    fn step_to_message_extracts_tool_calls() {
        let step = json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "content": "",
            "tool_calls": [
                {"name": "view_file", "args": {"AbsolutePath": "/tmp/data.txt"}}
            ]
        });
        let msg = step_to_message(&step).expect("tool-only step should map");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "view_file");
    }

    // -----------------------------------------------------------------------
    // Provider metadata + resume command
    // -----------------------------------------------------------------------

    #[test]
    fn provider_identity() {
        let p = Antigravity;
        assert_eq!(p.name(), "Antigravity CLI");
        assert_eq!(p.slug(), "antigravity");
        assert_eq!(p.cli_alias(), "agy");
        assert_eq!(p.write_refusal().is_none(), python_with_sdk().is_some());
        if let Some(refusal) = p.write_refusal() {
            assert_eq!(refusal, ANTIGRAVITY_SDK_REQUIRED);
        }
    }

    /// The resume command must be exactly what the shipped `agy` accepts.
    ///
    /// `--conversation` is the only flag `agy 1.1.7 --help` documents for
    /// resuming by id. It must not carry a model pin: `--model` takes a slug
    /// and `--effort` is a separate `low|medium|high` flag, so a combined
    /// display label like `"Gemini 3.1 Pro (High)"` resolves to nothing and
    /// hard-fails `-p` mode (agy 1.1.2), and ags has no source of truth for
    /// choosing a slug anyway.
    #[test]
    fn resume_command_is_conversation_id_only() {
        let p = Antigravity;
        let cmd =
            <Antigravity as Provider>::resume_command(&p, "901d1db7-8590-4cb0-a7cb-35fac369d860");
        assert_eq!(
            cmd,
            "agy --conversation 901d1db7-8590-4cb0-a7cb-35fac369d860"
        );
        assert!(
            !cmd.contains("--model"),
            "ags must not pin a model the user did not choose: {cmd}"
        );
        assert!(
            !cmd.contains("--effort"),
            "ags must not pin a reasoning effort the user did not choose: {cmd}"
        );
    }

    fn canonical(messages: Vec<CanonicalMessage>) -> CanonicalSession {
        CanonicalSession {
            session_id: "source".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages,
            metadata: serde_json::Value::Null,
            source_path: std::path::PathBuf::from("/tmp/source"),
            model_name: None,
        }
    }

    fn message(idx: usize, role: MessageRole, content: &str) -> CanonicalMessage {
        CanonicalMessage {
            idx,
            role,
            content: content.to_string(),
            timestamp: Some(1_700_000_000_000 + idx as i64),
            author: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            extra: serde_json::Value::Null,
        }
    }

    #[test]
    fn replay_projection_preserves_alternating_user_assistant_turns() {
        let session = canonical(vec![
            message(0, MessageRole::User, "question"),
            message(1, MessageRole::Assistant, "answer"),
            message(2, MessageRole::User, "follow-up"),
            message(3, MessageRole::Assistant, "second answer"),
        ]);
        let projection = project_replay(&session).expect("alternating session should replay");
        assert_eq!(projection.turns.len(), 2);
        assert_eq!(projection.turns[0].user, "question");
        assert_eq!(projection.turns[0].assistant, "answer");
        assert_eq!(projection.coalesced_user_messages, 0);
        assert_eq!(projection.coalesced_assistant_messages, 0);
    }

    #[test]
    fn replay_projection_labels_and_coalesces_adjacent_roles() {
        let session = canonical(vec![
            message(0, MessageRole::System, "follow policy"),
            message(1, MessageRole::User, "question"),
            message(2, MessageRole::Assistant, "answer"),
            message(3, MessageRole::Assistant, "more detail"),
        ]);
        let projection = project_replay(&session).expect("session should be projectable");
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.coalesced_user_messages, 1);
        assert_eq!(projection.coalesced_assistant_messages, 1);
        assert!(
            projection.turns[0]
                .user
                .contains("[ags imported system message]")
        );
        assert!(
            projection.turns[0]
                .user
                .contains("[ags imported user message]")
        );
        assert!(
            projection.turns[0]
                .assistant
                .contains("[ags imported assistant continuation]")
        );
    }

    #[test]
    fn replay_projection_refuses_trailing_unanswered_turn() {
        let session = canonical(vec![
            message(0, MessageRole::User, "question"),
            message(1, MessageRole::Assistant, "answer"),
            message(2, MessageRole::User, "unanswered"),
        ]);
        let error = project_replay(&session).expect_err("trailing user turn must be refused");
        assert!(
            error
                .to_string()
                .contains("without inventing an assistant reply")
        );
    }

    #[test]
    fn import_transcript_round_trips_all_role_buckets() {
        let session = canonical(vec![
            message(0, MessageRole::System, "system"),
            message(1, MessageRole::User, "user"),
            message(2, MessageRole::Assistant, "assistant"),
            message(3, MessageRole::Tool, "tool"),
        ]);
        let bytes = render_import_transcript(&session).expect("transcript should render");
        let mut readback = Vec::new();
        for line in String::from_utf8(bytes).expect("UTF-8 transcript").lines() {
            let step: serde_json::Value = serde_json::from_str(line).expect("JSON line");
            readback.push(step_to_message(&step).expect("message line"));
        }
        assert_eq!(
            readback
                .iter()
                .map(|message| (&message.role, message.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (&MessageRole::System, "system"),
                (&MessageRole::User, "user"),
                (&MessageRole::Assistant, "assistant"),
                (&MessageRole::Tool, "tool"),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // Enumeration + read from a fixture conversations/ + brain/ layout
    //
    // The crate is `#![forbid(unsafe_code)]`, so these unit tests must NOT
    // mutate process env (which requires `unsafe`). They build a real on-disk
    // `antigravity-cli/` tree and exercise the path-pure functions directly
    // (`list_conversations_in`, `read_session`). End-to-end env-driven
    // enumeration + gmi disambiguation is covered by the integration tests in
    // `tests/fixtures_test.rs`.
    // -----------------------------------------------------------------------

    /// Build a temporary `antigravity-cli` tree with conversations + brain
    /// transcripts, plus a sibling legacy gmi `tmp/.../chats` dir (which must be
    /// invisible to the agy enumerator). Returns `(tempdir guard, cli_dir)`.
    fn make_agy_tree(
        conversations: &[(&str, &str)], // (uuid, transcript_jsonl_contents)
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let home = tmp.path();
        let cli_dir = home.join("antigravity-cli");
        let conv_dir = cli_dir.join("conversations");
        std::fs::create_dir_all(&conv_dir).expect("create conversations dir");

        for (uuid, transcript) in conversations {
            // Conversation db (content opaque; only the filename stem matters).
            let db = conv_dir.join(format!("{uuid}.db"));
            std::fs::write(&db, b"SQLite format 3\x00fixture").expect("write db");

            // Brain transcript at the canonical sibling path.
            let logs = cli_dir
                .join("brain")
                .join(uuid)
                .join(".system_generated")
                .join("logs");
            std::fs::create_dir_all(&logs).expect("create logs dir");
            let mut f = std::fs::File::create(logs.join("transcript.jsonl")).expect("transcript");
            f.write_all(transcript.as_bytes())
                .expect("write transcript");
        }

        // Legacy gmi layout under the SAME ~/.gemini parent — MUST NOT be
        // picked up by the agy enumerator (it scans only conversations/*.db).
        let gmi_chats = home.join("tmp").join("deadbeefhash").join("chats");
        std::fs::create_dir_all(&gmi_chats).expect("create gmi chats");
        std::fs::write(
            gmi_chats.join("session-gmi-legacy-001.json"),
            br#"{"sessionId":"gmi-legacy-001","messages":[]}"#,
        )
        .expect("write gmi session");

        (tmp, cli_dir)
    }

    const SAMPLE_TRANSCRIPT: &str = concat!(
        r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-11T20:14:42Z","content":"<USER_REQUEST>\nRead data.txt\n</USER_REQUEST>"}"#,
        "\n",
        r#"{"step_index":1,"source":"SYSTEM","type":"CONVERSATION_HISTORY","status":"DONE","created_at":"2026-06-11T20:14:42Z"}"#,
        "\n",
        r#"{"step_index":2,"source":"SYSTEM","type":"EPHEMERAL_MESSAGE","status":"DONE","created_at":"2026-06-11T20:14:42Z","content":"noise"}"#,
        "\n",
        r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-11T20:15:10Z","content":"The answer is 1234.","thinking":"reasoned"}"#,
    );

    #[test]
    fn list_conversations_enumerates_db_stems_only() {
        let (_guard, cli_dir) = make_agy_tree(&[
            ("901d1db7-8590-4cb0-a7cb-35fac369d860", SAMPLE_TRANSCRIPT),
            ("ad053acc-0ee5-4f9b-b8b6-20506bfd5f56", SAMPLE_TRANSCRIPT),
        ]);

        let convs = list_conversations_in(&cli_dir.join("conversations")).sessions;
        let mut ids: Vec<String> = convs.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "901d1db7-8590-4cb0-a7cb-35fac369d860".to_string(),
                "ad053acc-0ee5-4f9b-b8b6-20506bfd5f56".to_string(),
            ]
        );
        // Crucially, the legacy gmi session id is NOT present.
        assert!(!ids.iter().any(|id| id.contains("gmi-legacy")));
        // Every enumerated path is a `.db` under conversations/.
        for (uuid, path) in &convs {
            assert!(path.ends_with(format!("{uuid}.db")));
        }
    }

    #[test]
    fn list_conversations_ignores_non_db_files() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let conv_dir = tmp.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).expect("mkdir");
        std::fs::write(conv_dir.join("real-uuid.db"), b"SQLite format 3\x00").expect("db");
        std::fs::write(conv_dir.join("notes.txt"), b"ignore me").expect("txt");
        std::fs::write(conv_dir.join("session-x.json"), b"{}").expect("json");

        let convs = list_conversations_in(&conv_dir).sessions;
        let ids: Vec<String> = convs.into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["real-uuid".to_string()]);
    }

    #[test]
    fn read_session_parses_transcript_and_leaves_model_unknown() {
        let (_guard, cli_dir) =
            make_agy_tree(&[("901d1db7-8590-4cb0-a7cb-35fac369d860", SAMPLE_TRANSCRIPT)]);
        let db = cli_dir
            .join("conversations")
            .join("901d1db7-8590-4cb0-a7cb-35fac369d860.db");
        let session = Antigravity.read_session(&db).expect("should read");

        assert_eq!(session.provider_slug, "antigravity");
        assert_eq!(session.session_id, "901d1db7-8590-4cb0-a7cb-35fac369d860");
        // No transcript step records a model, so the model is genuinely
        // unknown and must be reported as unknown rather than guessed.
        assert_eq!(
            session.model_name, None,
            "agy transcripts carry no model; ags must not invent one"
        );
        // Housekeeping SYSTEM steps dropped; only user + model remain.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Read data.txt");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "The answer is 1234.");
        assert_eq!(session.title.as_deref(), Some("Read data.txt"));
        assert!(session.started_at.is_some());
        assert!(session.ended_at.is_some());
        assert!(session.ended_at.unwrap() >= session.started_at.unwrap());
        // Sequential reindexing.
        for (i, m) in session.messages.iter().enumerate() {
            assert_eq!(m.idx, i);
        }
        // Metadata records the conversation uuid + transcript path.
        assert_eq!(
            session.metadata["conversation_uuid"].as_str(),
            Some("901d1db7-8590-4cb0-a7cb-35fac369d860")
        );
    }

    #[test]
    fn read_session_without_transcript_yields_empty_preview() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let conv_dir = tmp.path().join("antigravity-cli").join("conversations");
        std::fs::create_dir_all(&conv_dir).expect("create dirs");
        let db = conv_dir.join("no-brain-uuid.db");
        std::fs::write(&db, b"SQLite format 3\x00").expect("write db");

        let session = Antigravity.read_session(&db).expect("read should succeed");
        assert_eq!(session.session_id, "no-brain-uuid");
        assert_eq!(session.messages.len(), 0);
        assert!(session.title.is_none());
        // With no transcript there is even less to read a model off.
        assert_eq!(session.model_name, None);
    }
}
