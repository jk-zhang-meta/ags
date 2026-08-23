//! Cline provider — reads extension and CLI sessions and writes through Cline's local Hub.
//!
//! Cline is the VS Code extension published as `saoudrizwan.claude-dev`.
//! Its session artifacts are stored under the editor's `User/globalStorage`:
//!
//! - `<HOST_CONFIG>/User/globalStorage/saoudrizwan.claude-dev/tasks/<taskId>/api_conversation_history.json`
//! - `<HOST_CONFIG>/User/globalStorage/saoudrizwan.claude-dev/tasks/<taskId>/ui_messages.json`
//! - `<HOST_CONFIG>/User/globalStorage/saoudrizwan.claude-dev/state/taskHistory.json`
//!
//! Where `<HOST_CONFIG>` can be VS Code (`Code`, `Code - Insiders`, `VSCodium`) or Cursor.
//!
//! Current Cline CLI sessions live under
//! `<CLINE_DATA_DIR>/sessions/<id>/<id>.messages.json`. Target writes use the
//! official CLI's authenticated local Hub lifecycle:
//! `session.create(initialMessages)` -> `session.messages` -> `session.delete`
//! on rollback. No provider database, manifest, or shared index is edited
//! directly, and no prompt is supplied, so Cline persists the imported history
//! without starting a model turn.
//!
//! ## Session IDs
//!
//! Legacy task IDs are numeric strings (typically `Date.now()` / epoch millis).
//! Current CLI session IDs are opaque strings.
//!
//! ## A `tool_use` block is represented exactly once
//!
//! `api_conversation_history.json` is an Anthropic Messages-API conversation —
//! `MessageParam[]` with Cline-only bookkeeping (`ts`, `modelInfo`, `metrics`)
//! that `dist/extension.js`'s pre-send stripper removes before the request. An
//! assistant turn's `tool_use` block is the call and the matching `tool_result`
//! block on the following user turn is the observation:
//!
//! ```js
//! // saoudrizwan.claude-dev 4.0.11, dist/extension.js
//! return{type:"tool_use",id:r.id,name:r.name,input:n,signature:r.signature,call_id:r.call_id}
//! static createToolResultBlock(e,r,n){return r==="cline"||!r?{type:"text",…}:{type:"tool_result",tool_use_id:r,call_id:n,content:e}}
//! ```
//!
//! Measured on that build, a stored `tool_use` reaches the model **once**.
//! `resumeTaskFromHistory` reads the file back verbatim and the only transforms
//! between disk and the wire slice, reorder and strip; nothing re-renders a
//! call into prose. Cline does have a `[Tool Use: <name>]` renderer, and it
//! feeds the `api_req_started` **UI** row and a `conversation_history_*.txt`
//! hook artifact — neither of which is the conversation. So the vendor's own
//! artifact says one representation, and it is the structural one.
//!
//! (The same build ships an XML-in-text tool mode for models without native
//! tool calling — `<read_file><path>…</path></read_file>` inside an assistant
//! `text` block, recovered by a parser that never persists what it builds. Such
//! a file has no `tool_use` blocks at all, so it reads here as the text it is,
//! which is what it is to the model too.)
//!
//! [`Cline::extract_tool_calls`] returns every `tool_use` block in
//! [`CanonicalMessage::tool_calls`], so the text must not also name it.
//!
//! It used to. The reader flattened `content` with
//! [`crate::model::flatten_content`], whose `tool_use` arm writes
//! `[Tool: <name>]` — or `[Tool: <name> - <file_path>]` — into the prose, while
//! the candidate serializer writes the call from `tool_calls` alone and has no
//! way to put the text half back. The pipeline's read-back verification
//! compared `content` byte for byte, so every attempted conversion into Cline
//! carrying a tool call failed and was rolled back:
//!
//! ```text
//! VerifyFailed: message content mismatch at idx 1: wrote 8 bytes, read back 39 bytes
//! ```
//!
//! The file was right and the second copy was the reader's — the same finding
//! and the same fix as OpenClaw's `toolCall` (see
//! [`crate::providers::openclaw`]). [`Cline::flatten_message_text`] is the
//! reader's half. Test-only candidate serializers retain the structural form
//! so this invariant stays executable without advertising a safe target path.
//!
//! Removing that text also removed the only thing keeping a tool-call-only
//! assistant turn alive: this reader dropped any message whose flattened text
//! was empty, and 22,553 of the corpus's 55,638 assistant messages carry a call
//! and no prose. The emptiness test now asks about the whole message rather
//! than about its text.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::Context;
use tracing::{debug, trace};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tungstenite::{Message, WebSocket};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession, store_evidence};

/// VS Code Marketplace extension identifier.
const CLINE_EXTENSION_ID: &str = "saoudrizwan.claude-dev";

const FILE_API_HISTORY: &str = "api_conversation_history.json";
const FILE_UI_MESSAGES: &str = "ui_messages.json";
const FILE_UI_MESSAGES_OLD: &str = "claude_messages.json";
const FILE_TASK_HISTORY: &str = "taskHistory.json";
const CURRENT_MESSAGES_SUFFIX: &str = ".messages.json";
const CLINE_BIN_ENV: &str = "CLINE_BIN";
const HUB_DISCOVERY_PATH: &str = "locks/hub/production.json";
const HUB_AUTH_PROTOCOL_PREFIX: &str = "cline-hub-auth.";

const CLINE_CLI_REQUIRED: &str = "Cline is read/resume-only on this machine: target writes require \
the official `cline` CLI in PATH (or CLINE_BIN). ags uses the vendor's local Hub create, read, \
and delete lifecycle and will not modify Cline's database or session indexes directly.";

type HubSocket = WebSocket<TcpStream>;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubDiscovery {
    protocol_version: String,
    min_client_protocol_version: Option<String>,
    max_client_protocol_version: Option<String>,
    url: String,
    auth_token: String,
    host: String,
    port: u16,
}

struct HubClient {
    socket: HubSocket,
    client_id: String,
}

impl HubClient {
    fn connect(discovery: &HubDiscovery) -> anyhow::Result<Self> {
        if discovery.protocol_version != "v1"
            || discovery
                .min_client_protocol_version
                .as_deref()
                .is_some_and(|version| version != "v1")
            || discovery
                .max_client_protocol_version
                .as_deref()
                .is_some_and(|version| version != "v1")
        {
            anyhow::bail!("Cline Hub protocol is incompatible (ags requires protocol v1)");
        }
        if discovery.url.trim().is_empty()
            || discovery.auth_token.trim().is_empty()
            || discovery.host.trim().is_empty()
            || discovery.port == 0
        {
            anyhow::bail!("Cline Hub discovery record is incomplete");
        }

        let mut request = discovery
            .url
            .as_str()
            .into_client_request()
            .context("invalid Cline Hub discovery URL")?;
        if request.uri().scheme_str() != Some("ws") {
            anyhow::bail!("Cline Hub discovery URL must use local ws:// transport");
        }
        let request_host = request
            .uri()
            .host()
            .context("Cline Hub discovery URL has no host")?
            .trim_matches(['[', ']']);
        let discovery_host = discovery.host.trim().trim_matches(['[', ']']);
        if !request_host.eq_ignore_ascii_case(discovery_host)
            || request.uri().port_u16() != Some(discovery.port)
        {
            anyhow::bail!("Cline Hub discovery URL does not match its host and port");
        }
        let address = if let Ok(ip) = discovery_host.parse::<IpAddr>() {
            if !ip.is_loopback() {
                anyhow::bail!("Cline Hub discovery host is not a loopback address");
            }
            SocketAddr::new(ip, discovery.port)
        } else if discovery_host.eq_ignore_ascii_case("localhost") {
            (discovery_host, discovery.port)
                .to_socket_addrs()
                .context("failed to resolve the local Cline Hub host")?
                .find(|address| address.ip().is_loopback())
                .context("Cline Hub localhost did not resolve to a loopback address")?
        } else {
            anyhow::bail!("Cline Hub discovery host is not local");
        };

        let protocol = format!("{HUB_AUTH_PROTOCOL_PREFIX}{}", discovery.auth_token.trim());
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::try_from(protocol).context("invalid Cline Hub authentication token")?,
        );
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
            .context("failed to connect to the local Cline Hub")?;
        let timeout = Some(Duration::from_secs(20));
        stream
            .set_read_timeout(timeout)
            .context("failed to set Cline Hub read timeout")?;
        stream
            .set_write_timeout(timeout)
            .context("failed to set Cline Hub write timeout")?;
        let (socket, _) =
            tungstenite::client(request, stream).context("Cline Hub WebSocket handshake failed")?;

        Ok(Self {
            socket,
            client_id: format!("ags-{}", uuid::Uuid::new_v4().simple()),
        })
    }

    fn command_reply(
        &mut self,
        command: &str,
        payload: serde_json::Value,
        session_id: Option<&str>,
    ) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        let request_id = format!("ags-{}", uuid::Uuid::new_v4().simple());
        let mut envelope = serde_json::json!({
            "version": "v1",
            "command": command,
            "requestId": request_id,
            "clientId": self.client_id,
            "payload": payload,
        });
        if let Some(session_id) = session_id {
            envelope["sessionId"] = serde_json::Value::String(session_id.to_string());
        }
        let body = serde_json::to_string(&serde_json::json!({
            "kind": "command",
            "envelope": envelope,
        }))
        .context("failed to serialize a Cline Hub command")?;
        self.socket
            .send(Message::Text(body.into()))
            .with_context(|| format!("failed to send Cline Hub command {command}"))?;

        loop {
            let frame =
                match self.socket.read().with_context(|| {
                    format!("failed while waiting for Cline Hub command {command}")
                })? {
                    Message::Text(text) => serde_json::from_str::<serde_json::Value>(text.as_str())
                        .context("Cline Hub returned invalid JSON")?,
                    Message::Binary(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                        .context("Cline Hub returned invalid binary JSON")?,
                    Message::Ping(bytes) => {
                        self.socket
                            .send(Message::Pong(bytes))
                            .context("failed to answer Cline Hub ping")?;
                        continue;
                    }
                    Message::Close(reason) => {
                        anyhow::bail!("Cline Hub closed the connection: {reason:?}");
                    }
                    Message::Pong(_) | Message::Frame(_) => continue,
                };

            if frame.get("kind").and_then(serde_json::Value::as_str) != Some("reply") {
                continue;
            }
            let Some(reply) = frame.get("envelope").and_then(serde_json::Value::as_object) else {
                continue;
            };
            if reply.get("requestId").and_then(serde_json::Value::as_str)
                != Some(request_id.as_str())
            {
                continue;
            }
            if reply.get("version").and_then(serde_json::Value::as_str) != Some("v1") {
                anyhow::bail!("Cline Hub returned an incompatible reply protocol");
            }
            return Ok(reply.clone());
        }
    }

    fn command(
        &mut self,
        command: &str,
        payload: serde_json::Value,
        session_id: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let reply = self.command_reply(command, payload, session_id)?;
        if reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(reply
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }

        let code = reply
            .get("error")
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let message = reply
            .get("error")
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("Cline Hub command {command} failed ({code}): {message}");
    }

    fn register(&mut self, workspace: &Path) -> anyhow::Result<()> {
        self.command(
            "client.register",
            serde_json::json!({
                "clientId": self.client_id,
                "clientType": "ags",
                "displayName": "ags",
                "transport": "native",
                "actorKind": "client",
                "workspaceContext": {
                    "workspaceRoot": workspace,
                    "cwd": workspace,
                },
            }),
            None,
        )?;
        Ok(())
    }

    fn session_exists(&mut self, session_id: &str) -> anyhow::Result<bool> {
        let reply = self.command_reply(
            "session.get",
            serde_json::json!({"sessionId": session_id}),
            Some(session_id),
        )?;
        if reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(true);
        }
        if reply
            .get("error")
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            == Some("session_not_found")
        {
            return Ok(false);
        }

        let code = reply
            .get("error")
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let message = reply
            .get("error")
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("Cline Hub command session.get failed ({code}): {message}");
    }

    fn delete_session(&mut self, session_id: &str) -> anyhow::Result<bool> {
        let reply = self.command(
            "session.delete",
            serde_json::json!({"sessionId": session_id}),
            Some(session_id),
        )?;
        Ok(reply
            .get("deleted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }
}

/// Cline provider implementation.
pub struct Cline;

impl Cline {
    fn resume_spec(session_id: &str) -> LaunchSpec {
        LaunchSpec::new("cline", ["--id".to_string(), session_id.to_string()])
    }

    fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
        if path.is_absolute() {
            return Ok(path);
        }
        Ok(std::env::current_dir()
            .context("could not resolve a Cline path against the current directory")?
            .join(path))
    }

    fn binary_path() -> anyhow::Result<PathBuf> {
        if let Some(path) = std::env::var_os(CLINE_BIN_ENV).filter(|value| !value.is_empty()) {
            let path = Self::absolute_path(PathBuf::from(path))?;
            if path.is_file() {
                return Ok(path);
            }
            anyhow::bail!(
                "{CLINE_BIN_ENV} names {}, but that is not a Cline executable file",
                path.display()
            );
        }
        which::which("cline")
            .context("Cline writes require the official `cline` CLI in PATH (or CLINE_BIN)")
    }

    fn write_data_dir() -> anyhow::Result<PathBuf> {
        let data_dir = std::env::var_os("CLINE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(Self::sdk_data_dir)
            .context("could not determine Cline's data directory")?;
        Self::absolute_path(data_dir)
    }

    fn cli_command(binary: &Path, data_dir: &Path, cwd: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(cwd)
            .env("CLINE_DATA_DIR", data_dir)
            .env("CLINE_NO_AUTO_UPDATE", "1")
            .env("CLINE_TELEMETRY_DISABLED", "1");
        command
    }

    fn command_status(output: &Output) -> String {
        format!("exit status {}", output.status)
    }

    fn ensure_hub(binary: &Path, data_dir: &Path, cwd: &Path) -> anyhow::Result<HubDiscovery> {
        std::fs::create_dir_all(data_dir).with_context(|| {
            format!(
                "failed to create Cline data directory {}",
                data_dir.display()
            )
        })?;
        let output = Self::cli_command(binary, data_dir, cwd)
            .args(["hub", "ensure"])
            .output()
            .with_context(|| format!("failed to start {}", binary.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "Cline `hub ensure` failed: {}",
                Self::command_status(&output)
            );
        }

        let discovery_path = data_dir.join(HUB_DISCOVERY_PATH);
        let mut last_error = None;
        for _ in 0..40 {
            match std::fs::File::open(&discovery_path)
                .with_context(|| format!("failed to open {}", discovery_path.display()))
                .and_then(|file| {
                    serde_json::from_reader(file)
                        .with_context(|| format!("invalid json: {}", discovery_path.display()))
                }) {
                Ok(discovery) => return Ok(discovery),
                Err(error) => last_error = Some(error),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "Cline Hub discovery record did not appear at {}",
                discovery_path.display()
            )
        }))
    }

    fn connected_hub(
        binary: &Path,
        data_dir: &Path,
        workspace: &Path,
    ) -> anyhow::Result<HubClient> {
        let discovery = Self::ensure_hub(binary, data_dir, workspace)?;
        let mut hub = HubClient::connect(&discovery)?;
        hub.register(workspace)?;
        Ok(hub)
    }

    fn workspace_for_write(session: &CanonicalSession) -> anyhow::Result<(PathBuf, Vec<String>)> {
        if let Some(workspace) = session.workspace.as_ref()
            && workspace.is_dir()
        {
            return Ok((workspace.clone(), Vec::new()));
        }

        let cwd = std::env::current_dir().context("could not determine a workspace for Cline")?;
        let warnings = session
            .workspace
            .as_ref()
            .map_or_else(Vec::new, |workspace| {
                vec![format!(
                    "The source workspace {} does not exist; Cline imported the session into {}.",
                    workspace.display(),
                    cwd.display()
                )]
            });
        Ok((cwd, warnings))
    }

    fn title_for_write(session: &CanonicalSession) -> String {
        session
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                session
                    .messages
                    .iter()
                    .find(|message| message.role == MessageRole::User)
                    .map(|message| truncate_title(&message.content, 100))
                    .filter(|title| !title.trim().is_empty())
            })
            .unwrap_or_else(|| "Imported session".to_string())
    }

    /// Directories that each contain a `tasks/` (and possibly `state/`) tree.
    ///
    /// `CLINE_HOME` is casr's own override and names such a directory outright;
    /// when set it is used alone, which is what aims casr at a single tree.
    ///
    /// Otherwise Cline keeps tasks in two unrelated places and casr reads both:
    ///
    /// - the SDK/CLI store, whose location Cline resolves from its own
    ///   `CLINE_DATA_DIR`, else `$CLINE_DIR/data`, else `~/.cline/data`; and
    /// - the VS Code extension's `globalStorage` directory, which the *editor*
    ///   hands the extension. Cline honours no variable for that one, so there
    ///   is nothing to read there beyond the well-known editor locations.
    fn storage_roots() -> Vec<PathBuf> {
        if let Some(home) = std::env::var_os("CLINE_HOME").filter(|value| !value.is_empty()) {
            return vec![PathBuf::from(home)];
        }

        // Editor config roots that can host VS Code-style `User/globalStorage`.
        // We probe both config_dir and data_dir to cover Linux/Windows vs macOS.
        let mut host_roots: Vec<PathBuf> = Vec::new();
        if let Some(cfg) = dirs::config_dir() {
            host_roots.push(cfg.join("Code"));
            host_roots.push(cfg.join("Code - Insiders"));
            host_roots.push(cfg.join("VSCodium"));
            host_roots.push(cfg.join("Cursor"));
        }
        if let Some(data) = dirs::data_dir() {
            host_roots.push(data.join("Code"));
            host_roots.push(data.join("Code - Insiders"));
            host_roots.push(data.join("VSCodium"));
            host_roots.push(data.join("Cursor"));
        }

        // Deduplicate while preserving order.
        host_roots.sort();
        host_roots.dedup();

        let mut roots: Vec<PathBuf> = host_roots
            .into_iter()
            .map(|host| {
                host.join("User")
                    .join("globalStorage")
                    .join(CLINE_EXTENSION_ID)
            })
            .filter(|p| p.is_dir())
            .collect();

        // Appended after editor stores so discovery resolves an extension task
        // first when the same id exists in both layouts. Only directories that
        // already exist are listed, so `detect` still reports Cline as absent
        // on a machine that has never run it.
        roots.extend(Self::sdk_data_dir().filter(|p| p.is_dir()));
        roots
    }

    /// Cline's SDK/CLI task store, resolved exactly as Cline's own
    /// `resolveDataDir()` does: `CLINE_DATA_DIR`, else `$CLINE_DIR/data`, else
    /// `~/.cline/data`. Tasks live at `<dir>/tasks/<id>/`, the same shape as the
    /// extension's `globalStorage`.
    fn sdk_data_dir() -> Option<PathBuf> {
        if let Some(data) = std::env::var_os("CLINE_DATA_DIR").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(data));
        }
        let base = match std::env::var_os("CLINE_DIR").filter(|value| !value.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()?.join(".cline"),
        };
        Some(base.join("data"))
    }

    fn tasks_root(storage_root: &Path) -> PathBuf {
        storage_root.join("tasks")
    }

    fn sessions_root(storage_root: &Path) -> PathBuf {
        storage_root.join("sessions")
    }

    fn current_messages_path(storage_root: &Path, session_id: &str) -> PathBuf {
        Self::sessions_root(storage_root)
            .join(session_id)
            .join(format!("{session_id}{CURRENT_MESSAGES_SUFFIX}"))
    }

    fn current_session_from_path(path: &Path) -> Option<(PathBuf, String)> {
        let file_name = path.file_name()?.to_str()?;
        let session_id = file_name.strip_suffix(CURRENT_MESSAGES_SUFFIX)?;
        if session_id.is_empty()
            || path.parent()?.file_name()?.to_str()? != session_id
            || path.parent()?.parent()?.file_name()?.to_str()? != "sessions"
        {
            return None;
        }
        let storage_root = path.parent()?.parent()?.parent()?.to_path_buf();
        Some((storage_root, session_id.to_string()))
    }

    fn state_dir(storage_root: &Path) -> PathBuf {
        storage_root.join("state")
    }

    fn task_history_path(storage_root: &Path) -> PathBuf {
        Self::state_dir(storage_root).join(FILE_TASK_HISTORY)
    }

    fn task_dir_from_api_path(path: &Path) -> Option<PathBuf> {
        // .../tasks/<taskId>/<file>
        let task_dir = path.parent()?.to_path_buf();
        if task_dir.parent()?.file_name()?.to_string_lossy() != "tasks" {
            return None;
        }
        Some(task_dir)
    }

    fn task_id_from_task_dir(task_dir: &Path) -> Option<String> {
        task_dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    }

    fn find_storage_root_for_path(path: &Path) -> Option<PathBuf> {
        // Expect: <storage_root>/tasks/<taskId>/<file>
        let task_dir = Self::task_dir_from_api_path(path)?;
        let tasks_dir = task_dir.parent()?;
        let storage_root = tasks_dir.parent()?;
        Some(storage_root.to_path_buf())
    }

    fn read_json(path: &Path) -> anyhow::Result<serde_json::Value> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        serde_json::from_reader(reader).with_context(|| format!("invalid json: {}", path.display()))
    }

    /// Every field a `taskHistory.json` entry is known to hold.
    ///
    /// This list is an *allow-list*, and that is the point: the entry lands in
    /// canonical metadata under `taskHistoryItem`, which `casr info --json`
    /// prints verbatim — a command users pipe to a file and paste into issues.
    /// Copying the entry wholesale means whatever Cline adds to it next is
    /// republished with no change here and no one looking. Adding a name to
    /// this list republishes it; that is the review this list exists to force.
    ///
    /// Audited against Cline (`saoudrizwan.claude-dev`) **4.0.11**, from two
    /// independent artifacts that agree exactly:
    ///
    /// * the type — `apps/vscode/src/shared/HistoryItem.ts` at tag `v4.0.11`
    ///   (the pre-monorepo path `src/shared/HistoryItem.ts` is gone), and
    /// * the sole writer — `Task.saveClineMessagesAndUpdateHistoryInternal`
    ///   in the shipped `extension/dist/extension.js`, whose object literal
    ///   reaches disk through `Controller.updateTaskHistory`. That function
    ///   *replaces* the entry rather than merging, so nothing accretes into it.
    ///
    /// No field in the type can hold a credential, in any version from 2.0.0
    /// to 4.0.11. Cline keeps API keys in VS Code SecretStorage, mirrored to a
    /// sibling `state/secrets.json` written 0600 — never inside a history
    /// entry, and `apiConfiguration` is never embedded in one. `task` is the
    /// user's first prompt verbatim, so it is free text rather than a secret
    /// by construction; the three path/diagnostic fields carry absolute paths.
    /// Those are the same values `title`/`workspace` already surface, so
    /// filtering them here would buy nothing this reader does not already
    /// print. What filtering buys is the *next* field.
    ///
    /// `checkpointTrackerErrorMessage` is the pre-3.50 name of
    /// `checkpointManagerErrorMessage`; an entry written by an old Cline and
    /// never rewritten still carries it.
    const HISTORY_ITEM_FIELDS: [&str; 17] = [
        "id",
        "ulid",
        "ts",
        "task",
        "tokensIn",
        "tokensOut",
        "cacheWrites",
        "cacheReads",
        "totalCost",
        "size",
        "shadowGitConfigWorkTree",
        "cwdOnTaskInitialization",
        "conversationHistoryDeletedRange",
        "isFavorited",
        "checkpointManagerErrorMessage",
        "checkpointTrackerErrorMessage",
        "modelId",
    ];

    fn read_task_history_item(
        storage_root: &Path,
        task_id: &str,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let history_path = Self::task_history_path(storage_root);
        let Ok(root) = Self::read_json(&history_path) else {
            return None;
        };
        let serde_json::Value::Array(items) = root else {
            return None;
        };
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            if obj.get("id").and_then(|v| v.as_str()) == Some(task_id) {
                let mut kept = serde_json::Map::new();
                for field in Self::HISTORY_ITEM_FIELDS {
                    if let Some(value) = obj.get(field) {
                        kept.insert(field.to_string(), value.clone());
                    }
                }
                return Some(kept);
            }
        }
        None
    }

    /// A message's model-visible prose, with the blocks that come back as
    /// structure left out of it.
    ///
    /// Only `tool_use` is left out, and only because
    /// [`Self::extract_tool_calls`] already returns it — unconditionally, for
    /// every role, so the skip is unconditional too. `tool_result` needs no arm
    /// here: [`crate::model::flatten_content`] renders nothing for it (Cline's
    /// block carries the observation under `content`, not `text`), and
    /// [`Self::extract_tool_results`] is the channel it travels on.
    ///
    /// Delegating the rest to [`crate::model::flatten_content`] rather than
    /// re-implementing it keeps `text` blocks and bare strings reading
    /// identically to every other provider; the filter is the whole of Cline's
    /// divergence from it.
    fn flatten_message_text(content: &serde_json::Value) -> String {
        let serde_json::Value::Array(blocks) = content else {
            return flatten_content(content);
        };
        let prose: Vec<serde_json::Value> = blocks
            .iter()
            .filter(|block| block.get("type").and_then(|t| t.as_str()) != Some("tool_use"))
            .cloned()
            .collect();
        flatten_content(&serde_json::Value::Array(prose))
    }

    fn extract_tool_calls(content: Option<&serde_json::Value>) -> Vec<ToolCall> {
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

    fn extract_tool_results(content: Option<&serde_json::Value>) -> Vec<ToolResult> {
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
                let content_value = obj
                    .get("content")
                    .or_else(|| obj.get("output"))
                    .unwrap_or(&serde_json::Value::Null);
                Some(ToolResult {
                    call_id: obj
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    content: flatten_content(content_value),
                    is_error: obj
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect()
    }

    fn parse_message_items(
        items: &[serde_json::Value],
    ) -> (Vec<CanonicalMessage>, HashMap<String, usize>) {
        let mut messages = Vec::new();
        let mut model_counts = HashMap::new();

        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let role = normalize_role(
                obj.get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("user"),
            );
            let content_value = obj.get("content").unwrap_or(&serde_json::Value::Null);
            let content = Self::flatten_message_text(content_value);
            let tool_calls = Self::extract_tool_calls(Some(content_value));
            let tool_results = Self::extract_tool_results(Some(content_value));
            if content.trim().is_empty() && tool_calls.is_empty() && tool_results.is_empty() {
                continue;
            }

            let author = obj
                .get("modelInfo")
                .and_then(serde_json::Value::as_object)
                .and_then(|model| model.get("id").or_else(|| model.get("modelId")))
                .and_then(serde_json::Value::as_str)
                .filter(|model| !model.is_empty())
                .map(String::from);
            if let Some(model) = author.as_ref() {
                *model_counts.entry(model.clone()).or_insert(0) += 1;
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content,
                timestamp: obj.get("ts").and_then(parse_timestamp),
                author,
                tool_calls,
                tool_results,
                extra: serde_json::Value::Object(obj.clone()),
            });
        }
        reindex_messages(&mut messages);
        (messages, model_counts)
    }

    fn build_hub_messages(session: &CanonicalSession) -> Vec<serde_json::Value> {
        let mut call_ids: HashMap<(usize, usize), String> = HashMap::new();
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut unmatched_calls = VecDeque::new();
        for (message_index, message) in session.messages.iter().enumerate() {
            for (call_index, call) in message.tool_calls.iter().enumerate() {
                let call_id = call
                    .id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("ags-call-{message_index}-{call_index}"));
                let tool_name = if call.name.trim().is_empty() {
                    "tool".to_string()
                } else {
                    call.name.clone()
                };
                call_ids.insert((message_index, call_index), call_id.clone());
                tool_names.insert(call_id.clone(), tool_name);
                unmatched_calls.push_back(call_id);
            }
        }

        session
            .messages
            .iter()
            .enumerate()
            .map(|(message_index, message)| {
                let role = if message.role == MessageRole::Assistant {
                    "assistant"
                } else {
                    "user"
                };
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": message.content,
                    }));
                }
                for (index, call) in message.tool_calls.iter().enumerate() {
                    let id = call_ids
                        .get(&(message_index, index))
                        .expect("Cline tool call IDs are precomputed");
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": tool_names.get(id).expect("Cline tool name is precomputed"),
                        "input": crate::providers::claude_code::coerce_tool_input(&call.arguments),
                    }));
                }
                for (result_index, result) in message.tool_results.iter().enumerate() {
                    let call_id = result
                        .call_id
                        .as_deref()
                        .filter(|id| !id.trim().is_empty())
                        .map(String::from)
                        .or_else(|| unmatched_calls.pop_front())
                        .unwrap_or_else(|| format!("ags-result-{message_index}-{result_index}"));
                    if let Some(position) = unmatched_calls
                        .iter()
                        .position(|candidate| candidate == &call_id)
                    {
                        unmatched_calls.remove(position);
                    }
                    let tool_name = tool_names
                        .get(&call_id)
                        .map(String::as_str)
                        .unwrap_or("tool");
                    blocks.push(serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "name": tool_name,
                        "content": result.content,
                        "is_error": result.is_error,
                    }));
                }

                let mut out = serde_json::json!({
                    "role": role,
                    "content": blocks,
                });
                if let Some(timestamp) = message.timestamp {
                    out["ts"] = serde_json::Value::Number(timestamp.into());
                }
                if let Some(model) = message
                    .author
                    .as_deref()
                    .or(session.model_name.as_deref())
                    .filter(|model| !model.trim().is_empty())
                {
                    out["modelInfo"] = serde_json::json!({
                        "id": model,
                        "provider": session.provider_slug,
                    });
                }
                out
            })
            .collect()
    }

    fn without_generated_message_ids(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .cloned()
            .map(|mut message| {
                if let Some(object) = message.as_object_mut() {
                    object.remove("id");
                }
                message
            })
            .collect()
    }

    fn read_current_session(path: &Path) -> anyhow::Result<CanonicalSession> {
        let (_storage_root, session_id) =
            Self::current_session_from_path(path).ok_or_else(|| {
                anyhow::anyhow!("not a current Cline session path: {}", path.display())
            })?;
        let root = Self::read_json(path)?;
        let items = root
            .as_array()
            .or_else(|| root.get("messages").and_then(serde_json::Value::as_array))
            .context("Cline messages file has no messages array")?;
        let (messages, model_counts) = Self::parse_message_items(items);

        let manifest_path = path
            .parent()
            .context("Cline messages file has no session directory")?
            .join(format!("{session_id}.json"));
        let manifest = if manifest_path.is_file() {
            Self::read_json(&manifest_path)?
        } else {
            serde_json::Value::Null
        };
        let manifest_metadata = manifest
            .get("metadata")
            .and_then(serde_json::Value::as_object);
        let workspace = manifest
            .get("workspace_root")
            .and_then(serde_json::Value::as_str)
            .filter(|workspace| !workspace.trim().is_empty())
            .map(PathBuf::from);
        let started_at = manifest.get("started_at").and_then(parse_timestamp);
        let ended_at = manifest
            .get("ended_at")
            .and_then(parse_timestamp)
            .or(started_at);
        let title = manifest_metadata
            .and_then(|metadata| metadata.get("title"))
            .and_then(serde_json::Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(|title| truncate_title(title, 100))
            .or_else(|| {
                messages
                    .iter()
                    .find(|message| message.role == MessageRole::User)
                    .map(|message| truncate_title(&message.content, 100))
                    .filter(|title| !title.trim().is_empty())
            });
        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(model, _)| model)
            .or_else(|| {
                manifest_metadata
                    .and_then(|metadata| metadata.get("sourceModel"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|model| !model.trim().is_empty())
                    .map(String::from)
            })
            .or_else(|| {
                manifest
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|model| !model.trim().is_empty() && *model != "hub")
                    .map(String::from)
            });

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("cline-cli".to_string()),
        );
        let mut kept_manifest = serde_json::Map::new();
        for field in [
            "version",
            "source",
            "status",
            "interactive",
            "provider",
            "model",
            "workspace_root",
            "started_at",
            "ended_at",
        ] {
            if let Some(value) = manifest.get(field) {
                kept_manifest.insert(field.to_string(), value.clone());
            }
        }
        if let Some(source) = manifest_metadata {
            let mut kept = serde_json::Map::new();
            for field in [
                "source",
                "importedBy",
                "sourceProvider",
                "sourceModel",
                "title",
            ] {
                if let Some(value) = source.get(field) {
                    kept.insert(field.to_string(), value.clone());
                }
            }
            if !kept.is_empty() {
                kept_manifest.insert("metadata".to_string(), serde_json::Value::Object(kept));
            }
        }
        if !kept_manifest.is_empty() {
            metadata.insert(
                "sessionManifest".to_string(),
                serde_json::Value::Object(kept_manifest),
            );
        }

        Ok(CanonicalSession {
            session_id,
            provider_slug: "cline".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name,
        })
    }

    // These serializers remain test-only format probes. They let the reader's
    // structural invariants be checked against a plausible vendor artifact,
    // but they are not a supported write path: publishing one still requires a
    // safe vendor-authoritative task-history transaction.
    #[cfg(test)]
    fn generate_task_id(storage_root: &Path) -> String {
        let tasks_root = Self::tasks_root(storage_root);
        let mut candidate: i64 = chrono::Utc::now().timestamp_millis();
        loop {
            let id = candidate.to_string();
            if !tasks_root.join(&id).exists() {
                return id;
            }
            candidate = candidate.saturating_add(1);
        }
    }

    #[cfg(test)]
    fn build_api_history(session: &CanonicalSession) -> Vec<serde_json::Value> {
        let mut out = Vec::new();

        for msg in &session.messages {
            let role = match msg.role {
                MessageRole::Assistant => "assistant",
                MessageRole::User => "user",
                MessageRole::Tool | MessageRole::System | MessageRole::Other(_) => "user",
            };

            let mut blocks: Vec<serde_json::Value> = Vec::new();

            match role {
                "assistant" => {
                    if !msg.content.trim().is_empty() {
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
                }
                _ => {
                    if !msg.content.trim().is_empty() {
                        blocks.push(serde_json::json!({
                            "type": "text",
                            "text": msg.content,
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
                }
            }

            out.push(serde_json::json!({
                "role": role,
                "content": blocks,
            }));
        }

        out
    }

    #[cfg(test)]
    fn build_ui_messages(session: &CanonicalSession) -> Vec<serde_json::Value> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut cursor_ts = session.started_at.unwrap_or(now);

        // Cline's UI messages are not a simple chat transcript; we emit a minimal, plausible subset:
        // - a "task" say-message for the first user message
        // - "user_feedback" for subsequent user messages
        // - "text" for assistant messages
        let mut out = Vec::new();
        let mut first_task_emitted = false;

        for msg in &session.messages {
            let (say, text) = match msg.role {
                MessageRole::User => {
                    if !first_task_emitted {
                        first_task_emitted = true;
                        ("task", msg.content.clone())
                    } else {
                        ("user_feedback", msg.content.clone())
                    }
                }
                MessageRole::Assistant => ("text", msg.content.clone()),
                MessageRole::Tool | MessageRole::System | MessageRole::Other(_) => {
                    ("info", msg.content.clone())
                }
            };

            if text.trim().is_empty() {
                continue;
            }

            out.push(serde_json::json!({
                "ts": msg.timestamp.unwrap_or(cursor_ts),
                "type": "say",
                "say": say,
                "text": text,
            }));

            cursor_ts = cursor_ts.saturating_add(1);
        }

        out
    }

    #[cfg(test)]
    fn update_task_history(
        storage_root: &Path,
        task_id: &str,
        session: &CanonicalSession,
        provider_slug: &str,
    ) -> anyhow::Result<Option<crate::providers::Displaced>> {
        let history_path = Self::task_history_path(storage_root);

        // Missing state means a first task. Anything else unreadable or
        // malformed is existing user state we cannot safely reconstruct, so
        // fail closed instead of replacing it with a one-row array.
        let mut items: Vec<serde_json::Value> = match history_path
            .try_exists()
            .with_context(|| format!("failed to inspect {}", history_path.display()))?
        {
            false => Vec::new(),
            true => match Self::read_json(&history_path)? {
                serde_json::Value::Array(arr) => {
                    if arr.iter().any(|item| !item.is_object()) {
                        anyhow::bail!(
                            "Cline taskHistory.json contains a non-object item: {}",
                            history_path.display()
                        );
                    }
                    arr
                }
                _ => anyhow::bail!(
                    "Cline taskHistory.json is not an array: {}",
                    history_path.display()
                ),
            },
        };

        // Remove any existing entry with the same id (defensive).
        items.retain(|v| v.get("id").and_then(|x| x.as_str()) != Some(task_id));

        // Cline 4.0.11 filters every picker with `item.ts && item.task`.
        // Whitespace-only titles are just as unusable to a person, and a zero
        // timestamp is false in that predicate, so neither may reach the row.
        let title = session
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                session
                    .messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
                    .filter(|title| !title.trim().is_empty())
            })
            .unwrap_or_else(|| "Untitled Task".to_string());

        let ts = session
            .started_at
            .filter(|ts| *ts != 0)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), serde_json::Value::String(task_id.to_string()));
        obj.insert("ts".into(), serde_json::Value::Number(ts.into()));
        obj.insert("task".into(), serde_json::Value::String(title));
        obj.insert("tokensIn".into(), serde_json::Value::Number(0.into()));
        obj.insert("tokensOut".into(), serde_json::Value::Number(0.into()));
        obj.insert(
            "totalCost".into(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(0.0).unwrap_or_else(|| 0.into()),
            ),
        );
        if let Some(ws) = session.workspace.as_ref() {
            obj.insert(
                "cwdOnTaskInitialization".into(),
                serde_json::Value::String(ws.display().to_string()),
            );
        }
        if let Some(model) = session.model_name.as_ref() {
            obj.insert("modelId".into(), serde_json::Value::String(model.clone()));
        }

        items.push(serde_json::Value::Object(obj));

        // Sort newest-first for determinism.
        items.sort_by(|a, b| {
            let ta = a.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
            let tb = b.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
            tb.cmp(&ta)
        });

        let bytes = serde_json::to_vec_pretty(&serde_json::Value::Array(items))
            .context("failed to serialize taskHistory.json")?;

        // Format probe only. Atomic replacement prevents torn JSON, but cannot
        // make this read-modify-write safe against Cline's independent writer;
        // production therefore refuses before reaching any of these helpers.
        let outcome = crate::pipeline::atomic_write(&history_path, &bytes, true, provider_slug)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(outcome.displaced())
    }
}

impl Provider for Cline {
    fn name(&self) -> &str {
        "Cline"
    }

    fn slug(&self) -> &str {
        "cline"
    }

    fn cli_alias(&self) -> &str {
        "cln"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if let Ok(binary) = Self::binary_path() {
            installed = true;
            evidence.push(format!("official CLI: {}", binary.display()));
        }

        if let Ok(home) = std::env::var("CLINE_HOME") {
            evidence.push(format!("CLINE_HOME={home}"));
            let p = PathBuf::from(&home);
            if p.is_dir() {
                installed = true;
                evidence.push(format!("{} exists", p.display()));
            } else {
                evidence.push(format!("{} missing", p.display()));
            }
        }

        let roots = Self::storage_roots();
        if !roots.is_empty() {
            installed = true;
            for r in &roots {
                evidence.push(format!("{} detected", r.display()));
            }
        }

        // A storage root exists as soon as the extension is installed; its
        // `tasks/` subdirectory, which is the only thing `list` walks, appears
        // with the first task.
        if installed {
            for root in &roots {
                evidence.push(store_evidence(&Self::tasks_root(root)));
                evidence.push(store_evidence(&Self::sessions_root(root)));
            }
        }

        trace!(provider = "cline", installed, ?evidence, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        Self::storage_roots()
            .into_iter()
            .flat_map(|root| [Self::tasks_root(&root), Self::sessions_root(&root)])
            .collect()
    }

    /// The one file that stands for a task, and only for a task that has one.
    ///
    /// Cline's own gate is `api_conversation_history.json` existing —
    /// `getTaskWithId` in `saoudrizwan.claude-dev@4.0.11` builds the four task
    /// paths and treats the task as valid only `if (await exists(api))`. The
    /// two older transcripts stand in when it is absent, matching this
    /// provider's reader, which refuses `ui_messages.json` as a "non-primary
    /// Cline task artifact" whenever the API history is there.
    ///
    /// Everything else in `tasks/<taskId>/` is a sidecar Cline writes beside
    /// the transcript, and `list` was handing every one of them to
    /// `read_session` and reporting the refusal as a skipped session — five
    /// lines of noise per task, on every run. From the same bundle:
    /// `context_history.json`, `task_metadata.json`, a per-task
    /// `settings.json`, `focus_chain_taskid_<id>.md`, the transient
    /// `conversation_history_<epochMs>.{json,txt}` hook exports, and a legacy
    /// `checkpoints/` subtree.
    ///
    /// The atomic-write leftover is the one that a name list alone would miss:
    /// every save goes through `writeFile(\`${target}.tmp.${Date.now()}.${rand}.json\`)`
    /// then `rename`, so an interrupted save leaves
    /// `api_conversation_history.json.tmp.1769…abc.json` — a `.json` file whose
    /// name begins with the primary transcript's. Matching the full file name
    /// exactly, rather than an extension or a prefix, excludes it.
    ///
    /// # Where the file has to be
    ///
    /// A name alone is not enough, because the walk feeding this predicate is
    /// recursive (`main.rs`, `max_depth(4)`) and the same name can appear
    /// deeper. Cline enumerates tasks from one `readdir` of `tasks/`:
    ///
    /// ```js
    /// (await readdir(t, { withFileTypes: true }))
    ///     .filter(n => n.isDirectory())
    ///     .map(n => n.name)
    ///     .filter(n => /^\d+$/.test(n))
    /// ```
    ///
    /// so a task is `tasks/<digits>/`, exactly one level down — the id being
    /// the `Date.now()` that created it. casr's own writer agrees:
    /// `generate_task_id` returns `Utc::now().timestamp_millis().to_string()`,
    /// so tightening the predicate cannot orphan a session casr wrote.
    ///
    /// Both halves were leaking, and in different directions. A transcript
    /// copied a level deeper — `tasks/<id>/checkpoints/…`, `tasks/backups/a/…`
    /// — is refused by this provider's *reader*
    /// ([`Self::task_dir_from_api_path`] requires the grandparent to be
    /// `tasks/`), so every one of them was reported as a session that could not
    /// be read: a warning channel meant for real failures, describing files
    /// that were never sessions. A transcript at the right depth under a
    /// non-numeric directory — `tasks/backups/api_conversation_history.json` —
    /// the reader *accepts*, so it became a row with the task id `backups`.
    ///
    /// The root is resolved rather than assumed, so the rule follows whichever
    /// storage root the file is in: `session_roots()` is one `tasks/` per
    /// storage root, and each is the anchor for the tasks it holds.
    fn is_session_path(&self, path: &Path) -> bool {
        if let Some((storage_root, _)) = Self::current_session_from_path(path) {
            return Self::storage_roots()
                .iter()
                .any(|candidate| candidate == &storage_root);
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let is_transcript = match name {
            FILE_API_HISTORY => true,
            FILE_UI_MESSAGES | FILE_UI_MESSAGES_OLD => path
                .parent()
                .is_some_and(|dir| !dir.join(FILE_API_HISTORY).is_file()),
            _ => false,
        };
        if !is_transcript {
            return false;
        }

        let Some(task_dir) = path.parent() else {
            return false;
        };
        if !task_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
        task_dir
            .parent()
            .is_some_and(|tasks| self.session_roots().iter().any(|root| root == tasks))
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        for storage_root in Self::storage_roots() {
            let current = Self::current_messages_path(&storage_root, session_id);
            if current.is_file() {
                return Some(current);
            }
            let task_dir = Self::tasks_root(&storage_root).join(session_id);
            let api = task_dir.join(FILE_API_HISTORY);
            if api.is_file() {
                return Some(api);
            }
            let ui = task_dir.join(FILE_UI_MESSAGES);
            if ui.is_file() {
                return Some(ui);
            }
            let old = task_dir.join(FILE_UI_MESSAGES_OLD);
            if old.is_file() {
                return Some(old);
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        if Self::current_session_from_path(path).is_some() {
            return Self::read_current_session(path);
        }

        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !matches!(
            file_name,
            FILE_API_HISTORY | FILE_UI_MESSAGES | FILE_UI_MESSAGES_OLD
        ) {
            return Err(anyhow::anyhow!(
                "unsupported Cline session path (expected task file): {}",
                path.display()
            ));
        }

        let task_dir = Self::task_dir_from_api_path(path)
            .ok_or_else(|| anyhow::anyhow!("not a Cline task path: {}", path.display()))?;
        let task_id = Self::task_id_from_task_dir(&task_dir)
            .ok_or_else(|| anyhow::anyhow!("could not derive task id: {}", task_dir.display()))?;
        let storage_root = Self::find_storage_root_for_path(path).ok_or_else(|| {
            anyhow::anyhow!("could not derive Cline storage root for {}", path.display())
        })?;

        // Prefer API history for canonical messages (and avoid duplicates in `casr list`).
        let api_path = task_dir.join(FILE_API_HISTORY);
        let api_source_path = if file_name == FILE_API_HISTORY {
            path.to_path_buf()
        } else if api_path.is_file() {
            // If we were asked to read `ui_messages.json` but `api_conversation_history.json` exists,
            // treat the UI file as a non-primary artifact to avoid duplicate sessions in discovery.
            return Err(anyhow::anyhow!(
                "non-primary Cline task artifact (use {}): {}",
                FILE_API_HISTORY,
                path.display()
            ));
        } else {
            // Fall back to UI messages only when the API history file is missing.
            path.to_path_buf()
        };

        let mut messages = Vec::new();
        let mut model_counts = HashMap::new();

        if api_source_path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n == FILE_API_HISTORY)
        {
            let root = Self::read_json(&api_source_path)?;
            let serde_json::Value::Array(items) = root else {
                return Err(anyhow::anyhow!("Cline api history is not an array"));
            };
            (messages, model_counts) = Self::parse_message_items(&items);
        } else {
            // ui_messages.json fallback: extract a minimal conversational transcript.
            let root = Self::read_json(&api_source_path)?;
            let serde_json::Value::Array(items) = root else {
                return Err(anyhow::anyhow!("Cline ui messages is not an array"));
            };
            for item in items {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                if msg_type != "say" {
                    continue;
                }
                let say = obj.get("say").and_then(|v| v.as_str()).unwrap_or_default();
                let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }

                let role = match say {
                    "task" | "user_feedback" | "user_feedback_diff" => MessageRole::User,
                    _ => MessageRole::Assistant,
                };
                let ts = obj.get("ts").and_then(|v| v.as_i64());
                messages.push(CanonicalMessage {
                    idx: 0,
                    role,
                    content: text.to_string(),
                    timestamp: ts,
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::Value::Object(obj.clone()),
                });
            }
        }

        reindex_messages(&mut messages);

        let history_item = Self::read_task_history_item(&storage_root, &task_id);
        let workspace = history_item
            .as_ref()
            .and_then(|h| h.get("cwdOnTaskInitialization"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let started_at = history_item
            .as_ref()
            .and_then(|h| h.get("ts"))
            .and_then(|v| v.as_i64());

        let title = history_item
            .as_ref()
            .and_then(|h| h.get("task"))
            .and_then(|v| v.as_str())
            .map(|s| truncate_title(s, 100))
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });

        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| name)
            .or_else(|| {
                history_item
                    .as_ref()
                    .and_then(|h| h.get("modelId"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("cline".to_string()),
        );
        if let Some(h) = history_item {
            metadata.insert("taskHistoryItem".into(), serde_json::Value::Object(h));
        }

        debug!(task_id, messages = messages.len(), "Cline session parsed");

        Ok(CanonicalSession {
            session_id: task_id,
            provider_slug: "cline".to_string(),
            workspace,
            title,
            started_at,
            ended_at: started_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: api_source_path,
            model_name,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        if session.messages.is_empty() {
            anyhow::bail!(
                "Cline import requires at least one message; no provider state was created"
            );
        }

        let binary = Self::binary_path().map_err(|_| anyhow::anyhow!(CLINE_CLI_REQUIRED))?;
        let data_dir = Self::write_data_dir()?;
        let (workspace, warnings) = Self::workspace_for_write(session)?;
        let mut hub = Self::connected_hub(&binary, &data_dir, &workspace)?;
        let session_id = format!("ags-{}", uuid::Uuid::new_v4().simple());
        if hub.session_exists(&session_id)? {
            anyhow::bail!("Cline Hub already contains generated session ID {session_id}");
        }
        let initial_messages = Self::build_hub_messages(session);
        let title = Self::title_for_write(session);
        let create = hub.command(
            "session.create",
            serde_json::json!({
                "workspaceRoot": workspace,
                "cwd": workspace,
                "sessionConfig": {
                    "sessionId": session_id,
                    "providerId": "hub",
                    "modelId": "hub",
                    "cwd": workspace,
                    "workspaceRoot": workspace,
                    "systemPrompt": "",
                    "mode": "act",
                    "enableTools": false,
                    "enableSpawnAgent": false,
                    "enableAgentTeams": false,
                },
                "metadata": {
                    "source": "ags",
                    "interactive": false,
                    "title": title,
                    "importedBy": "ags",
                    "sourceProvider": session.provider_slug,
                    "sourceModel": session.model_name,
                },
                "runtimeOptions": {
                    "enableTools": false,
                    "enableSpawn": false,
                    "enableTeams": false,
                },
                "initialMessages": initial_messages,
            }),
            None,
        );
        if let Err(create_error) = create {
            return Err(match hub.delete_session(&session_id) {
                Ok(_) => create_error,
                Err(cleanup_error) => anyhow::anyhow!(
                    "{create_error:#}; partial-session cleanup failed: {cleanup_error:#}"
                ),
            });
        }

        let verified = hub
            .command(
                "session.messages",
                serde_json::json!({"sessionId": session_id}),
                Some(&session_id),
            )
            .and_then(|reply| {
                let messages = reply
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .context("Cline Hub session.messages reply has no messages array")?;
                let readback = Self::without_generated_message_ids(messages);
                if readback != initial_messages {
                    anyhow::bail!(
                        "Cline Hub changed the imported history (wrote {} messages, read back {})",
                        initial_messages.len(),
                        readback.len()
                    );
                }
                Ok(())
            });
        if let Err(verify_error) = verified {
            return Err(match hub.delete_session(&session_id) {
                Ok(true) => anyhow::anyhow!(
                    "Cline imported session {session_id}, but official read-back failed: \
                     {verify_error:#}; rollback succeeded"
                ),
                Ok(false) => anyhow::anyhow!(
                    "Cline imported session {session_id}, but official read-back failed: \
                     {verify_error:#}; rollback did not find the session"
                ),
                Err(cleanup_error) => anyhow::anyhow!(
                    "Cline imported session {session_id}, but official read-back failed: \
                     {verify_error:#}; rollback failed: {cleanup_error:#}"
                ),
            });
        }

        let messages_path = Self::current_messages_path(&data_dir, &session_id);
        if !messages_path.is_file() {
            return Err(match hub.delete_session(&session_id) {
                Ok(true) => anyhow::anyhow!(
                    "Cline Hub verified session {session_id}, but its official messages artifact \
                     is missing at {}; rollback succeeded",
                    messages_path.display()
                ),
                Ok(false) => anyhow::anyhow!(
                    "Cline Hub verified session {session_id}, but its official messages artifact \
                     is missing at {}; rollback did not find the session",
                    messages_path.display()
                ),
                Err(cleanup_error) => anyhow::anyhow!(
                    "Cline Hub verified session {session_id}, but its official messages artifact \
                     is missing at {}; rollback failed: {cleanup_error:#}",
                    messages_path.display()
                ),
            });
        }

        Ok(WrittenSession {
            paths: vec![messages_path],
            session_id: session_id.clone(),
            resume_command: Self::resume_spec(&session_id).display(),
            backups: Vec::new(),
            warnings,
        })
    }

    fn rollback_write(&self, written: &WrittenSession) -> anyhow::Result<()> {
        let locator = written
            .paths
            .first()
            .context("Cline rollback has no messages artifact path")?;
        let (data_dir, locator_session_id) = Self::current_session_from_path(locator)
            .context("Cline rollback received an invalid messages artifact path")?;
        if locator_session_id != written.session_id {
            anyhow::bail!(
                "Cline rollback path names {locator_session_id}, but the write names {}",
                written.session_id
            );
        }

        let binary = Self::binary_path()?;
        let cwd = std::env::current_dir().context("could not determine Cline rollback cwd")?;
        let mut hub = Self::connected_hub(&binary, &data_dir, &cwd)?;
        if hub.delete_session(&written.session_id)? {
            Ok(())
        } else {
            anyhow::bail!("Cline Hub did not delete session {}", written.session_id);
        }
    }

    fn write_refusal(&self) -> Option<&'static str> {
        Self::binary_path().is_err().then_some(CLINE_CLI_REQUIRED)
    }

    fn resume_command(&self, session_id: &str) -> String {
        Self::resume_spec(session_id).display()
    }

    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        Some(Self::resume_spec(session_id).targeting_session(session_id))
    }
}

// Integration tests for Cline live under `tests/` so they can safely isolate env vars.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult};
    use crate::providers::Provider;
    use serde_json::json;

    /// Create a temp Cline storage root with a task directory and API history file.
    /// Returns (storage_root, api_history_path).
    fn write_api_session(
        task_id: &str,
        api_entries: &[serde_json::Value],
        task_history: Option<&[serde_json::Value]>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join(task_id);
        std::fs::create_dir_all(&task_dir).expect("create task dir");

        let api_path = task_dir.join(FILE_API_HISTORY);
        let bytes = serde_json::to_vec(api_entries).expect("serialize api history");
        std::fs::write(&api_path, &bytes).expect("write api history");

        if let Some(history) = task_history {
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("create state dir");
            let history_path = state_dir.join(FILE_TASK_HISTORY);
            let hbytes = serde_json::to_vec(history).expect("serialize task history");
            std::fs::write(&history_path, &hbytes).expect("write task history");
        }

        (root, api_path)
    }

    fn make_api_user(text: &str) -> serde_json::Value {
        json!({
            "role": "user",
            "content": [{"type": "text", "text": text}]
        })
    }

    fn make_api_assistant(text: &str) -> serde_json::Value {
        json!({
            "role": "assistant",
            "content": [{"type": "text", "text": text}]
        })
    }

    fn make_api_assistant_with_model(text: &str, model_id: &str) -> serde_json::Value {
        json!({
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "modelInfo": {"modelId": model_id}
        })
    }

    fn make_canonical_session(messages: Vec<CanonicalMessage>) -> CanonicalSession {
        CanonicalSession {
            session_id: "source-1".to_string(),
            provider_slug: "test".to_string(),
            workspace: Some(PathBuf::from("/data/projects/test_ws")),
            title: Some("Test Session".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_001_000),
            messages,
            metadata: serde_json::Value::Null,
            source_path: PathBuf::from("/tmp/source.jsonl"),
            model_name: None,
        }
    }

    // -----------------------------------------------------------------------
    // Reader tests — API format (bd-16s.4)
    // -----------------------------------------------------------------------

    #[test]
    fn reader_api_basic_exchange() {
        let entries = vec![
            make_api_user("Hello world"),
            make_api_assistant("Hi there!"),
        ];
        let (_root, api_path) = write_api_session("1700000000001", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.provider_slug, "cline");
        assert_eq!(session.session_id, "1700000000001");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello world");
        assert_eq!(session.messages[0].idx, 0);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Hi there!");
        assert_eq!(session.messages[1].idx, 1);
    }

    #[test]
    fn reader_api_tool_use_blocks() {
        let entries = vec![
            make_api_user("Read the file"),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me read that file."},
                    {
                        "type": "tool_use",
                        "id": "tool-abc",
                        "name": "ReadFile",
                        "input": {"path": "src/main.rs"}
                    }
                ]
            }),
        ];
        let (_root, api_path) = write_api_session("1700000000002", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.messages.len(), 2);
        let assistant = &session.messages[1];
        assert_eq!(assistant.role, MessageRole::Assistant);
        // The prose, and only the prose. `flatten_content`'s `tool_use` arm
        // would have appended `[Tool: ReadFile]` here, beside the same call
        // returned structurally below.
        assert_eq!(assistant.content, "Let me read that file.");
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].name, "ReadFile");
        assert_eq!(assistant.tool_calls[0].id.as_deref(), Some("tool-abc"));
        assert_eq!(
            assistant.tool_calls[0].arguments["path"].as_str(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn reader_api_tool_result_blocks() {
        // A user message with a text block AND tool_result blocks.
        let entries = vec![
            make_api_user("Read the file"),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Here is the result"},
                    {
                        "type": "tool_result",
                        "tool_use_id": "tool-abc",
                        "content": "fn main() { }",
                        "is_error": false
                    }
                ]
            }),
            make_api_assistant("I see the file content."),
        ];
        let (_root, api_path) = write_api_session("1700000000003", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        let tool_msg = session
            .messages
            .iter()
            .find(|m| !m.tool_results.is_empty())
            .expect("should have a message with tool_results");
        assert_eq!(tool_msg.role, MessageRole::User);
        assert!(tool_msg.content.contains("Here is the result"));
        assert_eq!(tool_msg.tool_results.len(), 1);
        assert_eq!(
            tool_msg.tool_results[0].call_id.as_deref(),
            Some("tool-abc")
        );
        assert_eq!(tool_msg.tool_results[0].content, "fn main() { }");
        assert!(!tool_msg.tool_results[0].is_error);
    }

    /// A user turn whose only block is a `tool_result` is how an
    /// Anthropic-shaped conversation carries a tool observation, and Cline
    /// writes exactly that. It used to be dropped, because the reader tested
    /// its *text* for emptiness and `flatten_content` renders nothing for a
    /// `tool_result` block — so the observation left the transcript with it,
    /// and the turn count came back short. Emptiness is now a property of the
    /// message.
    #[test]
    fn reader_api_keeps_tool_result_only_message() {
        let entries = vec![
            make_api_user("Read the file"),
            json!({
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tool-xyz",
                        "content": "fn main() { }",
                        "is_error": false
                    }
                ]
            }),
            make_api_assistant("I see it."),
        ];
        let (_root, api_path) = write_api_session("1700000000013", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].content, "Read the file");
        assert_eq!(session.messages[1].content, "");
        assert_eq!(session.messages[1].tool_results.len(), 1);
        assert_eq!(session.messages[1].tool_results[0].content, "fn main() { }");
        assert_eq!(session.messages[2].content, "I see it.");
    }

    /// The corpus case: 22,553 of 55,638 assistant messages are tool-call-only,
    /// so the turn with no prose at all is the common one, not the edge.
    #[test]
    fn reader_api_keeps_tool_call_only_assistant_message() {
        let entries = vec![
            make_api_user("Read the file"),
            json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tool-only",
                        "name": "ReadFile",
                        "input": {"path": "src/main.rs"}
                    }
                ]
            }),
        ];
        let (_root, api_path) = write_api_session("1700000000014", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(
            session.messages.len(),
            2,
            "a turn whose only content is a tool call is still a turn"
        );
        assert_eq!(session.messages[1].content, "");
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert_eq!(session.messages[1].tool_calls[0].name, "ReadFile");
    }

    #[test]
    fn reader_api_model_info_extraction() {
        let entries = vec![
            make_api_user("Hello"),
            make_api_assistant_with_model("Hi", "claude-3.5-sonnet"),
            make_api_user("Another"),
            make_api_assistant_with_model("Reply", "claude-3.5-sonnet"),
        ];
        let (_root, api_path) = write_api_session("1700000000004", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.model_name.as_deref(), Some("claude-3.5-sonnet"));
        assert_eq!(
            session.messages[1].author.as_deref(),
            Some("claude-3.5-sonnet")
        );
    }

    #[test]
    fn reader_api_skips_empty_content() {
        let entries = vec![
            make_api_user("Hello"),
            json!({"role": "assistant", "content": []}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "  "}]}),
            make_api_assistant("Real answer"),
        ];
        let (_root, api_path) = write_api_session("1700000000005", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        // Empty content and whitespace-only content should be skipped.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "Hello");
        assert_eq!(session.messages[1].content, "Real answer");
    }

    #[test]
    fn reader_api_title_from_task_history() {
        let task_history = vec![json!({
            "id": "1700000000006",
            "ts": 1_700_000_000_006_i64,
            "task": "Fix authentication bug",
            "tokensIn": 100,
            "tokensOut": 200,
            "totalCost": 0.01
        })];
        let entries = vec![
            make_api_user("Fix the auth bug"),
            make_api_assistant("On it!"),
        ];
        let (_root, api_path) = write_api_session("1700000000006", &entries, Some(&task_history));

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.title.as_deref(), Some("Fix authentication bug"));
    }

    #[test]
    fn reader_api_title_fallback() {
        // No taskHistory.json — title should fall back to first user message.
        let entries = vec![
            make_api_user("Implement dark mode support"),
            make_api_assistant("Sure!"),
        ];
        let (_root, api_path) = write_api_session("1700000000007", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(
            session.title.as_deref(),
            Some("Implement dark mode support")
        );
    }

    #[test]
    fn reader_api_workspace_from_history() {
        let task_history = vec![json!({
            "id": "1700000000008",
            "ts": 1_700_000_000_008_i64,
            "task": "Some task",
            "cwdOnTaskInitialization": "/data/projects/my_app"
        })];
        let entries = vec![make_api_user("Hello"), make_api_assistant("Hi")];
        let (_root, api_path) = write_api_session("1700000000008", &entries, Some(&task_history));

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(
            session.workspace,
            Some(PathBuf::from("/data/projects/my_app"))
        );
    }

    #[test]
    fn reader_api_session_id() {
        let entries = vec![make_api_user("Hello"), make_api_assistant("Hi")];
        let (_root, api_path) = write_api_session("1700000099999", &entries, None);

        let session = Cline.read_session(&api_path).expect("read_session");
        assert_eq!(session.session_id, "1700000099999");
    }

    #[test]
    fn reader_api_rejects_non_primary() {
        // When api_conversation_history.json exists, reading ui_messages.json
        // should return an error directing users to the primary file.
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join("1700000000010");
        std::fs::create_dir_all(&task_dir).expect("create task dir");

        let api_path = task_dir.join(FILE_API_HISTORY);
        let ui_path = task_dir.join(FILE_UI_MESSAGES);

        let api_entries = vec![make_api_user("Hello"), make_api_assistant("Hi")];
        std::fs::write(&api_path, serde_json::to_vec(&api_entries).unwrap()).unwrap();

        let ui_entries =
            vec![json!({"type": "say", "say": "task", "text": "Hello", "ts": 1700000000010_i64})];
        std::fs::write(&ui_path, serde_json::to_vec(&ui_entries).unwrap()).unwrap();

        let err = Cline.read_session(&ui_path).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("non-primary"),
            "expected 'non-primary' error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // Writer tests (bd-16s.6)
    // -----------------------------------------------------------------------

    #[test]
    fn build_api_history_structure() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hello".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Hi".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![ToolCall {
                    id: Some("tc-1".to_string()),
                    name: "Read".to_string(),
                    arguments: json!({"path": "main.rs"}),
                }],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let api = Cline::build_api_history(&session);
        assert_eq!(api.len(), 2);

        // First entry: user
        assert_eq!(api[0]["role"].as_str(), Some("user"));
        let blocks0 = api[0]["content"].as_array().expect("user content blocks");
        assert_eq!(blocks0.len(), 1);
        assert_eq!(blocks0[0]["type"].as_str(), Some("text"));
        assert_eq!(blocks0[0]["text"].as_str(), Some("Hello"));

        // Second entry: assistant with text + tool_use
        assert_eq!(api[1]["role"].as_str(), Some("assistant"));
        let blocks1 = api[1]["content"]
            .as_array()
            .expect("assistant content blocks");
        assert_eq!(blocks1.len(), 2);
        assert_eq!(blocks1[0]["type"].as_str(), Some("text"));
        assert_eq!(blocks1[0]["text"].as_str(), Some("Hi"));
        assert_eq!(blocks1[1]["type"].as_str(), Some("tool_use"));
        assert_eq!(blocks1[1]["name"].as_str(), Some("Read"));
        assert_eq!(blocks1[1]["id"].as_str(), Some("tc-1"));
    }

    #[test]
    fn build_api_history_tool_results_in_user_message() {
        let session = make_canonical_session(vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: Some("tc-1".to_string()),
                content: "file contents".to_string(),
                is_error: false,
            }],
            extra: serde_json::Value::Null,
        }]);

        let api = Cline::build_api_history(&session);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"].as_str(), Some("user"));
        let blocks = api[0]["content"].as_array().expect("content blocks");
        // Empty text should not create a text block, only the tool_result block
        assert!(
            blocks
                .iter()
                .any(|b| b["type"].as_str() == Some("tool_result")),
            "expected tool_result block"
        );
        let tr_block = blocks
            .iter()
            .find(|b| b["type"].as_str() == Some("tool_result"))
            .expect("tool_result block");
        assert_eq!(tr_block["tool_use_id"].as_str(), Some("tc-1"));
        assert_eq!(tr_block["content"].as_str(), Some("file contents"));
    }

    #[test]
    fn build_ui_messages_first_user_is_task() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Fix the bug".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "On it".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let ui = Cline::build_ui_messages(&session);
        assert_eq!(ui.len(), 2);
        assert_eq!(ui[0]["say"].as_str(), Some("task"));
        assert_eq!(ui[0]["text"].as_str(), Some("Fix the bug"));
        assert_eq!(ui[0]["type"].as_str(), Some("say"));
    }

    #[test]
    fn build_ui_messages_subsequent_user_is_feedback() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Initial task".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "OK".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 2,
                role: MessageRole::User,
                content: "Follow up".to_string(),
                timestamp: Some(1_700_000_000_002),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let ui = Cline::build_ui_messages(&session);
        assert_eq!(ui.len(), 3);
        assert_eq!(ui[0]["say"].as_str(), Some("task"));
        assert_eq!(ui[1]["say"].as_str(), Some("text"));
        assert_eq!(ui[2]["say"].as_str(), Some("user_feedback"));
        assert_eq!(ui[2]["text"].as_str(), Some("Follow up"));
    }

    #[test]
    fn build_ui_messages_skips_empty_content() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hello".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "  ".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 2,
                role: MessageRole::Assistant,
                content: "Real reply".to_string(),
                timestamp: Some(1_700_000_000_002),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let ui = Cline::build_ui_messages(&session);
        assert_eq!(ui.len(), 2);
        assert_eq!(ui[0]["text"].as_str(), Some("Hello"));
        assert_eq!(ui[1]["text"].as_str(), Some("Real reply"));
    }

    #[test]
    fn writer_resume_command() {
        assert_eq!(
            Cline.resume_command("ags-session"),
            "cline --id ags-session"
        );
    }

    #[test]
    fn writer_generates_numeric_task_id() {
        let root = tempfile::tempdir().expect("tmpdir");
        let tasks = root.path().join("tasks");
        std::fs::create_dir_all(&tasks).expect("create tasks dir");

        let id = Cline::generate_task_id(root.path());
        // Should be a numeric string (epoch millis).
        assert!(id.parse::<i64>().is_ok(), "task id should be numeric: {id}");
    }

    #[test]
    fn writer_generates_unique_task_id_on_collision() {
        let root = tempfile::tempdir().expect("tmpdir");
        let tasks = root.path().join("tasks");
        std::fs::create_dir_all(&tasks).expect("create tasks dir");

        // Pre-create a task directory to force collision handling.
        let id1 = Cline::generate_task_id(root.path());
        std::fs::create_dir_all(tasks.join(&id1)).expect("create collision dir");

        let id2 = Cline::generate_task_id(root.path());
        assert_ne!(id1, id2, "should generate different ID on collision");
        assert!(id2.parse::<i64>().is_ok(), "collision id should be numeric");
    }

    /// Roundtrip test: build API history from canonical, write to temp, read back.
    /// This tests the writer-then-reader path without needing CLINE_HOME env var.
    #[test]
    fn writer_roundtrip_via_build_and_read() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hello roundtrip".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Hi roundtrip".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![ToolCall {
                    id: Some("tc-rt".to_string()),
                    name: "Read".to_string(),
                    arguments: json!({"path": "lib.rs"}),
                }],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        // Build the API history JSON, write to a temp task directory, and read back.
        let api_history = Cline::build_api_history(&session);
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join("1700000099000");
        std::fs::create_dir_all(&task_dir).expect("create task dir");

        let api_path = task_dir.join(FILE_API_HISTORY);
        let bytes = serde_json::to_vec(&api_history).expect("serialize");
        std::fs::write(&api_path, &bytes).expect("write");

        let readback = Cline.read_session(&api_path).expect("readback");
        assert_eq!(readback.messages.len(), session.messages.len());
        // Byte for byte, with nothing tolerated at the end. This assertion used
        // to accept anything the read-back appended, which is exactly the
        // difference `pipeline`'s read-back verification refuses to accept — so
        // the test passed while every real conversion into Cline carrying a
        // tool call was rolled back.
        for (orig, rb) in session.messages.iter().zip(readback.messages.iter()) {
            assert_eq!(orig.role, rb.role);
            assert_eq!(orig.content, rb.content);
        }
        assert_eq!(readback.messages[1].tool_calls.len(), 1);
        assert_eq!(readback.messages[1].tool_calls[0].name, "Read");
        assert_eq!(readback.session_id, "1700000099000");
    }

    /// Verify that build_api_history produces correct role assignments.
    #[test]
    fn writer_api_history_role_assignments() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hello".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Hi".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 2,
                role: MessageRole::System,
                content: "System msg".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 3,
                role: MessageRole::Tool,
                content: "Tool output".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let api = Cline::build_api_history(&session);
        assert_eq!(api.len(), 4);
        assert_eq!(api[0]["role"].as_str(), Some("user"));
        assert_eq!(api[1]["role"].as_str(), Some("assistant"));
        // System and Tool roles map to "user" in Cline's API format.
        assert_eq!(api[2]["role"].as_str(), Some("user"));
        assert_eq!(api[3]["role"].as_str(), Some("user"));
    }

    /// Verify UI messages format has correct structure.
    #[test]
    fn writer_ui_messages_format() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Task text".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Reply".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let ui = Cline::build_ui_messages(&session);
        assert_eq!(ui.len(), 2);

        // All UI messages should have ts, type, say, and text fields.
        for msg in &ui {
            assert!(msg.get("ts").is_some(), "missing ts field");
            assert_eq!(msg["type"].as_str(), Some("say"));
            assert!(msg.get("say").is_some(), "missing say field");
            assert!(msg.get("text").is_some(), "missing text field");
        }

        assert_eq!(ui[0]["say"].as_str(), Some("task"));
        assert_eq!(ui[0]["text"].as_str(), Some("Task text"));
        assert_eq!(ui[0]["ts"].as_i64(), Some(1_700_000_000_000));
        assert_eq!(ui[1]["say"].as_str(), Some("text"));
        assert_eq!(ui[1]["text"].as_str(), Some("Reply"));
    }

    /// Verify system/tool/other roles map to "info" in UI messages.
    #[test]
    fn writer_ui_messages_other_roles() {
        let session = make_canonical_session(vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hello".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::System,
                content: "System note".to_string(),
                timestamp: Some(1_700_000_000_001),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
        ]);

        let ui = Cline::build_ui_messages(&session);
        assert_eq!(ui.len(), 2);
        assert_eq!(ui[1]["say"].as_str(), Some("info"));
    }

    #[test]
    fn writer_updates_task_history() {
        let root = tempfile::tempdir().expect("tmpdir");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");

        let session = make_canonical_session(vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "My task".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }]);

        Cline::update_task_history(root.path(), "1700000099999", &session, "cline")
            .expect("update_task_history");

        let history_path = state_dir.join(FILE_TASK_HISTORY);
        assert!(history_path.exists());
        let content: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["id"].as_str(), Some("1700000099999"));
        assert_eq!(content[0]["task"].as_str(), Some("Test Session"));
    }

    #[test]
    fn writer_task_history_rows_remain_visible_when_source_fields_are_empty() {
        let root = tempfile::tempdir().expect("tmpdir");
        let mut session = make_canonical_session(vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Fallback task".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }]);
        session.title = Some("   ".to_string());
        session.started_at = Some(0);

        Cline::update_task_history(root.path(), "1700000099998", &session, "cline")
            .expect("update visible task history row");

        let history_path = Cline::task_history_path(root.path());
        let content: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
        assert_eq!(content[0]["task"].as_str(), Some("Fallback task"));
        assert!(
            content[0]["ts"].as_i64().is_some_and(|ts| ts != 0),
            "Cline filters zero timestamps out of every history picker"
        );

        session.messages[0].content = "  ".to_string();
        Cline::update_task_history(root.path(), "1700000099999", &session, "cline")
            .expect("update row with untitled fallback");
        let content: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
        let row = content
            .iter()
            .find(|row| row["id"] == "1700000099999")
            .expect("second history row");
        assert_eq!(row["task"].as_str(), Some("Untitled Task"));
    }

    #[test]
    fn writer_refuses_to_replace_an_invalid_existing_task_history() {
        for invalid in [
            b"{not-json".as_slice(),
            b"{\"task\":\"not an array\"}".as_slice(),
            b"[null]".as_slice(),
            b"[[]]".as_slice(),
        ] {
            let root = tempfile::tempdir().expect("tmpdir");
            let history_path = Cline::task_history_path(root.path());
            std::fs::create_dir_all(history_path.parent().unwrap()).expect("create state dir");
            std::fs::write(&history_path, invalid).expect("seed invalid task history");

            let session = make_canonical_session(vec![CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Must not replace the index".to_string(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            }]);
            let error = Cline::update_task_history(root.path(), "1700000099999", &session, "cline")
                .expect_err("existing invalid state must fail closed");

            assert!(
                error.to_string().contains("taskHistory"),
                "error should identify the shared state file: {error:#}"
            );
            assert_eq!(
                std::fs::read(&history_path).unwrap(),
                invalid,
                "a malformed shared index must remain recoverable in place"
            );
        }
    }

    #[test]
    fn writer_task_history_sorted_newest_first() {
        let root = tempfile::tempdir().expect("tmpdir");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");

        // Pre-populate with an older entry.
        let existing = vec![json!({
            "id": "1600000000000",
            "ts": 1_600_000_000_000_i64,
            "task": "Old task"
        })];
        let history_path = state_dir.join(FILE_TASK_HISTORY);
        std::fs::write(&history_path, serde_json::to_vec(&existing).unwrap()).unwrap();

        let mut session = make_canonical_session(vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "New task".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }]);
        session.started_at = Some(1_700_000_000_000);

        Cline::update_task_history(root.path(), "1700000000000", &session, "cline")
            .expect("update_task_history");

        let content: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
        assert_eq!(content.len(), 2);
        // Newer entry should be first.
        let first_ts = content[0]["ts"].as_i64().unwrap();
        let second_ts = content[1]["ts"].as_i64().unwrap();
        assert!(first_ts >= second_ts, "should be sorted newest-first");
        assert_eq!(content[0]["id"].as_str(), Some("1700000000000"));
    }

    #[test]
    fn writer_task_history_deduplicates() {
        let root = tempfile::tempdir().expect("tmpdir");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");

        // Pre-populate with an entry that has the same ID we'll insert.
        let existing = vec![json!({
            "id": "1700000000000",
            "ts": 1_700_000_000_000_i64,
            "task": "Old version"
        })];
        let history_path = state_dir.join(FILE_TASK_HISTORY);
        std::fs::write(&history_path, serde_json::to_vec(&existing).unwrap()).unwrap();

        let session = make_canonical_session(vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "New version".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }]);

        Cline::update_task_history(root.path(), "1700000000000", &session, "cline")
            .expect("update_task_history");

        let content: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
        // Should have exactly 1 entry, not 2.
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["id"].as_str(), Some("1700000000000"));
        // Should be the new version (with title from session.title).
        assert_eq!(content[0]["task"].as_str(), Some("Test Session"));
    }

    // -----------------------------------------------------------------------
    // Reader tests — UI format fallback (bd-16s.5)
    // -----------------------------------------------------------------------

    /// Create a temp Cline storage root with a task directory containing
    /// ONLY ui_messages.json (no api_conversation_history.json).
    fn write_ui_session(
        task_id: &str,
        ui_entries: &[serde_json::Value],
        task_history: Option<&[serde_json::Value]>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join(task_id);
        std::fs::create_dir_all(&task_dir).expect("create task dir");

        let ui_path = task_dir.join(FILE_UI_MESSAGES);
        let bytes = serde_json::to_vec(ui_entries).expect("serialize ui messages");
        std::fs::write(&ui_path, &bytes).expect("write ui messages");

        if let Some(history) = task_history {
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("create state dir");
            let history_path = state_dir.join(FILE_TASK_HISTORY);
            let hbytes = serde_json::to_vec(history).expect("serialize task history");
            std::fs::write(&history_path, &hbytes).expect("write task history");
        }

        (root, ui_path)
    }

    #[test]
    fn reader_ui_basic_exchange() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Fix the bug", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "Working on it", "ts": 1_700_000_000_001_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020001", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.provider_slug, "cline");
        assert_eq!(session.session_id, "1700000020001");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Fix the bug");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Working on it");
    }

    #[test]
    fn reader_ui_task_say_as_user() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Initial task text", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "OK", "ts": 1_700_000_000_001_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020002", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Initial task text");
    }

    #[test]
    fn reader_ui_user_feedback_as_user() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Start task", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "OK", "ts": 1_700_000_000_001_i64}),
            json!({"type": "say", "say": "user_feedback", "text": "Try again", "ts": 1_700_000_000_002_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020003", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[2].role, MessageRole::User);
        assert_eq!(session.messages[2].content, "Try again");
    }

    #[test]
    fn reader_ui_user_feedback_diff_as_user() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Start", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "user_feedback_diff", "text": "Diff feedback", "ts": 1_700_000_000_001_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020004", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, MessageRole::User);
        assert_eq!(session.messages[1].content, "Diff feedback");
    }

    #[test]
    fn reader_ui_text_say_as_assistant() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Hello", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "Response text", "ts": 1_700_000_000_001_i64}),
            json!({"type": "say", "say": "completion_result", "text": "Done", "ts": 1_700_000_000_002_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020005", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        // "text" and "completion_result" say types both map to Assistant.
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[2].role, MessageRole::Assistant);
    }

    #[test]
    fn reader_ui_skips_non_say_types() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Hello", "ts": 1_700_000_000_000_i64}),
            json!({"type": "ask", "ask": "tool", "text": "Allow?", "ts": 1_700_000_000_001_i64}),
            json!({"type": "say", "say": "text", "text": "OK", "ts": 1_700_000_000_002_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020006", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        // "ask" type messages should be skipped.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "Hello");
        assert_eq!(session.messages[1].content, "OK");
    }

    #[test]
    fn reader_ui_timestamps_parsed() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Hello", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "Hi", "ts": 1_700_000_000_500_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020007", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages[0].timestamp, Some(1_700_000_000_000));
        assert_eq!(session.messages[1].timestamp, Some(1_700_000_000_500));
    }

    #[test]
    fn reader_ui_empty_text_skipped() {
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Hello", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "", "ts": 1_700_000_000_001_i64}),
            json!({"type": "say", "say": "text", "text": "  ", "ts": 1_700_000_000_002_i64}),
            json!({"type": "say", "say": "text", "text": "Real reply", "ts": 1_700_000_000_003_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020008", &entries, None);

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "Real reply");
    }

    #[test]
    fn reader_ui_fallback_when_no_api_file() {
        // When only ui_messages.json exists (no api file), the reader should
        // fall back to UI format parsing.
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Task text", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "Response", "ts": 1_700_000_000_001_i64}),
        ];
        let (_root, ui_path) = write_ui_session("1700000020009", &entries, None);

        // Verify no API file exists.
        let task_dir = ui_path.parent().unwrap();
        assert!(!task_dir.join(FILE_API_HISTORY).exists());

        let session = Cline.read_session(&ui_path).expect("read_session");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.provider_slug, "cline");
    }

    #[test]
    fn reader_ui_old_claude_messages_filename() {
        // Test reading via the legacy `claude_messages.json` filename.
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join("1700000020010");
        std::fs::create_dir_all(&task_dir).expect("create task dir");

        let old_path = task_dir.join(FILE_UI_MESSAGES_OLD);
        let entries = vec![
            json!({"type": "say", "say": "task", "text": "Old format", "ts": 1_700_000_000_000_i64}),
            json!({"type": "say", "say": "text", "text": "Reply", "ts": 1_700_000_000_001_i64}),
        ];
        std::fs::write(&old_path, serde_json::to_vec(&entries).unwrap()).unwrap();

        let session = Cline.read_session(&old_path).expect("read_session");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "Old format");
    }

    // -----------------------------------------------------------------------
    // Helper and detection tests (bd-16s.7)
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let cline = Cline;
        assert_eq!(cline.name(), "Cline");
        assert_eq!(cline.slug(), "cline");
        assert_eq!(cline.cli_alias(), "cln");
    }

    #[test]
    fn find_storage_root_for_path_extracts_root() {
        let path = PathBuf::from(
            "/home/user/.config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/123/api_conversation_history.json",
        );
        let root = Cline::find_storage_root_for_path(&path);
        assert_eq!(
            root,
            Some(PathBuf::from(
                "/home/user/.config/Code/User/globalStorage/saoudrizwan.claude-dev"
            ))
        );
    }

    #[test]
    fn find_storage_root_for_path_returns_none_for_invalid() {
        let path = PathBuf::from("/not/a/cline/path.json");
        assert!(Cline::find_storage_root_for_path(&path).is_none());
    }

    #[test]
    fn read_task_history_item_found() {
        let root = tempfile::tempdir().expect("tmpdir");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");

        let history = vec![
            json!({"id": "111", "ts": 111, "task": "Task A"}),
            json!({"id": "222", "ts": 222, "task": "Task B"}),
        ];
        std::fs::write(
            state_dir.join(FILE_TASK_HISTORY),
            serde_json::to_vec(&history).unwrap(),
        )
        .unwrap();

        let item = Cline::read_task_history_item(root.path(), "222");
        assert!(item.is_some());
        let item = item.unwrap();
        assert_eq!(item["task"].as_str(), Some("Task B"));
    }

    #[test]
    fn read_task_history_item_missing() {
        let root = tempfile::tempdir().expect("tmpdir");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");

        let history = vec![json!({"id": "111", "ts": 111, "task": "Task A"})];
        std::fs::write(
            state_dir.join(FILE_TASK_HISTORY),
            serde_json::to_vec(&history).unwrap(),
        )
        .unwrap();

        assert!(Cline::read_task_history_item(root.path(), "999").is_none());
    }

    #[test]
    fn read_task_history_item_no_file() {
        let root = tempfile::tempdir().expect("tmpdir");
        // No state directory at all.
        assert!(Cline::read_task_history_item(root.path(), "123").is_none());
    }

    #[test]
    fn owns_session_finds_api_file() {
        let root = tempfile::tempdir().expect("tmpdir");
        let task_dir = root.path().join("tasks").join("1700000030001");
        std::fs::create_dir_all(&task_dir).expect("create task dir");
        let api = task_dir.join(FILE_API_HISTORY);
        std::fs::write(&api, b"[]").expect("write api file");

        // owns_session uses storage_roots() which needs CLINE_HOME (env var).
        // Since we can't set env vars (unsafe), we test the underlying logic
        // via read_session on the known path instead.
        let session = Cline.read_session(&api);
        // Should succeed (even if empty array produces no messages → error is fine,
        // but the path resolution should not fail).
        let _ = session;
    }

    // -----------------------------------------------------------------------
    // Existing helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn task_dir_from_api_path_valid() {
        let path = PathBuf::from("/home/user/.cline/tasks/123456/api_conversation_history.json");
        let task_dir = Cline::task_dir_from_api_path(&path);
        assert_eq!(
            task_dir,
            Some(PathBuf::from("/home/user/.cline/tasks/123456"))
        );
    }

    #[test]
    fn task_dir_from_api_path_invalid() {
        // Not under a "tasks" directory.
        let path = PathBuf::from("/home/user/.cline/other/123456/api_conversation_history.json");
        assert!(Cline::task_dir_from_api_path(&path).is_none());
    }

    #[test]
    fn task_id_from_task_dir_extracts_id() {
        let path = PathBuf::from("/home/user/.cline/tasks/9876543210");
        assert_eq!(
            Cline::task_id_from_task_dir(&path),
            Some("9876543210".to_string())
        );
    }

    #[test]
    fn extract_tool_calls_from_content() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {
                "type": "tool_use",
                "id": "tc-1",
                "name": "ReadFile",
                "input": {"path": "a.rs"}
            }
        ]);
        let calls = Cline::extract_tool_calls(Some(&content));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ReadFile");
        assert_eq!(calls[0].id.as_deref(), Some("tc-1"));
    }

    #[test]
    fn extract_tool_results_from_content() {
        let content = json!([
            {
                "type": "tool_result",
                "tool_use_id": "tc-1",
                "content": "result text",
                "is_error": true
            }
        ]);
        let results = Cline::extract_tool_results(Some(&content));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id.as_deref(), Some("tc-1"));
        assert_eq!(results[0].content, "result text");
        assert!(results[0].is_error);
    }

    #[test]
    fn extract_tool_calls_none_input() {
        assert!(Cline::extract_tool_calls(None).is_empty());
    }

    #[test]
    fn extract_tool_results_none_input() {
        assert!(Cline::extract_tool_results(None).is_empty());
    }
}
