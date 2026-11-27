//! Unified Cross-Platform Pure-Rust Session Hub (`bh ls`, `bh resume`, `bh save`, `bh info`).
//!
//! Aggregates sessions across:
//! 1. Host provider homes (`~/.claude`, `~/.codex`, `~/.grok`, `~/.gemini/antigravity-cli`, etc.)
//! 2. ALL Berth Sandbox Cage homes (`<state_dir>/cages/*/home/{.claude,.codex,.grok,.gemini/antigravity-cli}`)
//! 3. Berth Cloud Vault (1 GiB/user default quota) + local/git/sftp remote stores
//!
//! Renders a blazing-fast pure-Rust `crossterm` interactive TUI across Linux/WSL2, macOS,
//! and native Windows (with panic-hook + signal terminal restoration safe under `panic = "abort"`).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use serde::{Deserialize, Serialize};

use crate::berth::{BerthConfig, BerthPaths, load_config, write_private_file_atomic};
use crate::bundle::{BundleDecryption, UnpackedBundle};
use crate::discovery::ProviderRegistry;
use crate::model::{CanonicalMessage, CanonicalSession, MessageRole, truncate_title};
use crate::providers::claude_code::ClaudeCode;
use crate::providers::codex::Codex;
use crate::providers::{Provider, WriteOptions};
use crate::sandbox::AccountCageHome;
use crate::vault_sync::{
    BerthQuotaInfo, StoreBackendConfig, VaultRecordSummary, berth_vault_delete,
    berth_vault_download, berth_vault_list, berth_vault_patch, default_age_identity_path,
    format_bytes_human, load_store_registry, local_cache_root, local_dir_delete,
    local_dir_get_bundle, local_dir_load_index, render_quota_bar, save_session_file_to_store,
};

const SESSION_LABELS_FILE: &str = "session-labels.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SessionOrigin {
    Host,
    Cage(String),
    CloudVault(String),
    LocalStore(String),
}

impl SessionOrigin {
    pub fn short_tag(&self) -> String {
        match self {
            Self::Host => "host".to_string(),
            Self::Cage(k) => format!("cage:{k}"),
            Self::CloudVault(_) => "cloud".to_string(),
            Self::LocalStore(s) => format!("store:{s}"),
        }
    }

    pub fn is_cloud(&self) -> bool {
        matches!(self, Self::CloudVault(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedSessionRow {
    pub session_id: String,
    pub provider: String,
    pub origin: SessionOrigin,
    pub title: String,
    #[serde(default)]
    pub custom_name: Option<String>,
    pub workspace: String,
    pub git_branch: String,
    pub message_count: usize,
    pub size_bytes: u64,
    pub updated_at: DateTime<Utc>,
    pub pinned: bool,
    pub encrypted: bool,
    pub in_cloud: bool,
    #[serde(default)]
    pub vault_id: Option<String>,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
}

impl UnifiedSessionRow {
    pub fn display_title(&self) -> String {
        let raw = self
            .custom_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.title);
        sanitize_terminal_text(raw)
    }

    pub fn short_provider(&self) -> &'static str {
        match self.provider.as_str() {
            "claude-code" | "claude" => "claude",
            "codex" => "codex",
            "antigravity" | "agy" => "agy",
            "grok" => "grok",
            "gemini" => "gemini",
            "opencode" => "opencode",
            _ => "other",
        }
    }

    pub fn label_key(&self) -> String {
        format!("{}:{}:{}", self.origin.short_tag(), self.provider, self.session_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalSessionLabel {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionLabelsStore {
    pub labels: BTreeMap<String, LocalSessionLabel>,
}

pub fn load_session_labels(paths: &BerthPaths) -> SessionLabelsStore {
    let f = paths.state_dir.join(SESSION_LABELS_FILE);
    fs::read_to_string(&f)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_session_labels(paths: &BerthPaths, store: &SessionLabelsStore) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let bytes = serde_json::to_vec_pretty(store)?;
    write_private_file_atomic(&paths.state_dir.join(SESSION_LABELS_FILE), &bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Strips ANSI escape sequences and ASCII control characters from untrusted titles
/// before writing to any terminal.
pub fn sanitize_terminal_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(n) = chars.next() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\n' || c == '\r' || c == '\t' {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Canonicalizes workspace spelling for reliable cross-OS / symlink / case comparison.
pub fn normalize_workspace_key(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return String::new();
    }
    let p = Path::new(trimmed);
    let resolved = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    resolved.to_string_lossy().replace('\\', "/").to_ascii_lowercase()
}

#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    pub rows: Vec<UnifiedSessionRow>,
    pub quota: Option<BerthQuotaInfo>,
    pub cloud_online: bool,
    pub cloud_error: Option<String>,
}

/// Discovers sessions across Host homes, ALL Berth Sandbox Cage homes, and Berth Cloud Vault.
pub fn discover_unified_sessions(
    paths: &BerthPaths,
    include_cloud: bool,
    provider_filter: Option<&str>,
) -> DiscoveryResult {
    let labels = load_session_labels(paths);
    let mut rows: Vec<UnifiedSessionRow> = Vec::new();
    let mut cloud_ids: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut quota = None;
    let mut cloud_online = false;
    let mut cloud_error = None;

    // 1. Fetch Berth Cloud Vault metadata (non-blocking fail-soft if offline or not logged in).
    if include_cloud {
        match berth_vault_list(paths, provider_filter, None) {
            Ok((items, q)) => {
                cloud_online = true;
                quota = Some(q);
                for item in items {
                    if !item.source_session_id.is_empty() {
                        cloud_ids.insert(
                            (item.provider.clone(), item.source_session_id.clone()),
                            item.vault_id.clone(),
                        );
                    }
                    rows.push(row_from_vault_summary(&item, SessionOrigin::CloudVault(item.vault_id.clone())));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("berth is not configured") {
                    cloud_error = Some(msg);
                }
            }
        }
    }

    // 2. Scan Host homes (Claude Code, Codex, Grok, Antigravity, Gemini, OpenCode, etc.).
    let host_home = dirs::home_dir();
    if let Some(ref home) = host_home {
        collect_provider_homes(
            home,
            &SessionOrigin::Host,
            provider_filter,
            &cloud_ids,
            &labels,
            &mut rows,
        );
    }

    // 3. Scan ALL Berth Sandbox Cage homes (`<state_dir>/cages/*/home`).
    let cages_root = paths.state_dir.join("cages");
    if let Ok(entries) = fs::read_dir(&cages_root) {
        for entry in entries.flatten() {
            let cage_dir = entry.path();
            if !cage_dir.is_dir() {
                continue;
            }
            let Some(acct_key) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let cage_home = cage_dir.join("home");
            if cage_home.is_dir() {
                collect_provider_homes(
                    &cage_home,
                    &SessionOrigin::Cage(acct_key),
                    provider_filter,
                    &cloud_ids,
                    &labels,
                    &mut rows,
                );
            }
        }
    }

    // 4. Include local vault cache records across all registered store caches.
    let mut existing_vids: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r.vault_id.clone())
        .collect();
    for (store_label, cache_dir) in crate::vault_sync::all_local_cache_roots(paths) {
        if let Ok(idx) = local_dir_load_index(&cache_dir) {
            for rec in idx.records.values() {
                if !existing_vids.contains(&rec.vault_id) {
                    if let Some(pf) = provider_filter {
                        if !matches_provider_filter(&rec.provider, pf) {
                            continue;
                        }
                    }
                    existing_vids.insert(rec.vault_id.clone());
                    rows.push(row_from_vault_summary(
                        rec,
                        SessionOrigin::LocalStore(store_label.clone()),
                    ));
                }
            }
        }
    }

    // Sort: pinned first, then most recently updated descending.
    rows.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    DiscoveryResult {
        rows,
        quota,
        cloud_online,
        cloud_error,
    }
}

fn collect_provider_homes(
    home_root: &Path,
    origin: &SessionOrigin,
    provider_filter: Option<&str>,
    cloud_ids: &std::collections::HashMap<(String, String), String>,
    labels: &SessionLabelsStore,
    out: &mut Vec<UnifiedSessionRow>,
) {
    // Claude Code
    if provider_filter.is_none_or(|f| matches_provider_filter("claude-code", f)) {
        let claude_dir = home_root.join(".claude");
        if claude_dir.is_dir() {
            let listing = ClaudeCode::list_sessions_in_home(&claude_dir);
            for (sid, path) in listing.sessions {
                if let Some(mut row) = quick_inspect_claude_jsonl(&sid, &path, origin.clone()) {
                    apply_cloud_and_labels(&mut row, cloud_ids, labels);
                    out.push(row);
                }
            }
        }
    }

    // Codex
    if provider_filter.is_none_or(|f| matches_provider_filter("codex", f)) {
        let codex_dir = home_root.join(".codex");
        if codex_dir.is_dir() {
            let db_titles = Codex::read_thread_titles_in_home(&codex_dir);
            let listing = Codex::list_sessions_in_home(&codex_dir);
            for (sid, path) in listing.sessions {
                if let Some(mut row) =
                    quick_inspect_codex_rollout(&sid, &path, origin.clone(), db_titles.get(&sid))
                {
                    apply_cloud_and_labels(&mut row, cloud_ids, labels);
                    out.push(row);
                }
            }
        }
    }

    // Grok
    if provider_filter.is_none_or(|f| matches_provider_filter("grok", f)) {
        let grok_sessions = home_root.join(".grok").join("sessions");
        if grok_sessions.is_dir() {
            collect_grok_sessions(&grok_sessions, origin, cloud_ids, labels, out);
        }
    }

    // Antigravity (`agy`)
    if provider_filter.is_none_or(|f| matches_provider_filter("antigravity", f)) {
        let agy_dir = home_root.join(".gemini").join("antigravity-cli");
        if agy_dir.is_dir() {
            collect_agy_sessions(&agy_dir, origin, cloud_ids, labels, out);
        }
    }
}

fn apply_cloud_and_labels(
    row: &mut UnifiedSessionRow,
    cloud_ids: &std::collections::HashMap<(String, String), String>,
    labels: &SessionLabelsStore,
) {
    if let Some(vid) = cloud_ids.get(&(row.provider.clone(), row.session_id.clone())) {
        row.in_cloud = true;
        row.vault_id = Some(vid.clone());
    }
    if let Some(lbl) = labels.labels.get(&row.label_key()) {
        if lbl.name.is_some() {
            row.custom_name = lbl.name.clone();
        }
        if lbl.pinned {
            row.pinned = true;
        }
    }
}

fn row_from_vault_summary(item: &VaultRecordSummary, origin: SessionOrigin) -> UnifiedSessionRow {
    let title = if item.encrypted {
        format!("[encrypted] {}", &item.vault_id[..item.vault_id.len().min(14)])
    } else if !item.title.trim().is_empty() {
        item.title.clone()
    } else {
        format!("{}/{}", item.provider, &item.logical_id)
    };
    UnifiedSessionRow {
        session_id: if !item.source_session_id.is_empty() {
            item.source_session_id.clone()
        } else {
            item.vault_id.clone()
        },
        provider: item.provider.clone(),
        origin,
        title,
        custom_name: None,
        workspace: item.workspace_hint.clone(),
        git_branch: item.git_branch.clone(),
        message_count: item.message_count.max(0) as usize,
        size_bytes: item.stored_bytes.max(0) as u64,
        updated_at: item.updated_at,
        pinned: item.pinned,
        encrypted: item.encrypted,
        in_cloud: true,
        vault_id: Some(item.vault_id.clone()),
        source_path: None,
    }
}

/// Fast head+tail inspector for Claude Code JSONL transcripts (avoids parsing 200 MiB files!).
fn quick_inspect_claude_jsonl(
    session_id: &str,
    path: &Path,
    origin: SessionOrigin,
) -> Option<UnifiedSessionRow> {
    let meta = fs::metadata(path).ok()?;
    let size_bytes = meta.len();
    if size_bytes == 0 {
        return None;
    }
    let updated_at = file_mtime_utc(&meta);
    let (head_lines, tail_lines) = read_head_and_tail_lines(path, 32, 64 * 1024).ok()?;

    let mut workspace = String::new();
    let mut git_branch = String::new();
    let mut custom_title = None;
    let mut ai_title = None;
    let mut away_title = None;
    let mut summary_title = None;
    let mut first_user = None;
    let mut msg_count = 0usize;

    for line in head_lines.iter().chain(tail_lines.iter()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if workspace.is_empty() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                workspace = c.to_string();
            }
        }
        if git_branch.is_empty() {
            if let Some(gb) = v.get("gitBranch").and_then(|x| x.as_str()) {
                if gb != "HEAD" {
                    git_branch = gb.to_string();
                }
            }
        }
        match v.get("type").and_then(|x| x.as_str()) {
            Some("custom-title") => {
                if let Some(t) = v.get("customTitle").and_then(|x| x.as_str()) {
                    custom_title = Some(t.to_string());
                }
            }
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    ai_title = Some(t.to_string());
                }
            }
            Some("system")
                if v.get("subtype").and_then(|x| x.as_str()) == Some("away_summary") =>
            {
                if let Some(t) = v
                    .get("content")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.get("summary").and_then(|x| x.as_str()))
                {
                    away_title = Some(truncate_title(t.lines().next().unwrap_or(t), 90));
                }
            }
            Some("summary") => {
                if let Some(t) = v.get("summary").and_then(|x| x.as_str()) {
                    summary_title = Some(t.to_string());
                }
            }
            Some("user") => {
                msg_count += 1;
                if first_user.is_none() {
                    if let Some(txt) = extract_quick_text(v.pointer("/message/content")) {
                        if !txt.trim().is_empty() {
                            first_user = Some(truncate_title(&txt, 90));
                        }
                    }
                }
            }
            Some("assistant") => {
                msg_count += 1;
            }
            _ => {}
        }
    }

    let title = custom_title
        .or(ai_title)
        .or(away_title)
        .or(summary_title)
        .or(first_user)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("claude/{}", &session_id[..session_id.len().min(12)]));

    Some(UnifiedSessionRow {
        session_id: session_id.to_string(),
        provider: "claude-code".to_string(),
        origin,
        title,
        custom_name: None,
        workspace,
        git_branch,
        message_count: msg_count,
        size_bytes,
        updated_at,
        pinned: false,
        encrypted: false,
        in_cloud: false,
        vault_id: None,
        source_path: Some(path.to_path_buf()),
    })
}

/// Fast head+SQLite inspector for Codex rollouts (supports `.jsonl` and `.jsonl.zst`).
fn quick_inspect_codex_rollout(
    session_id: &str,
    path: &Path,
    origin: SessionOrigin,
    db_row: Option<&(String, String, bool)>,
) -> Option<UnifiedSessionRow> {
    let meta = fs::metadata(path).ok()?;
    let size_bytes = meta.len();
    if size_bytes == 0 {
        return None;
    }
    let updated_at = file_mtime_utc(&meta);

    let mut title = db_row
        .map(|(t, _, _)| t.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_default();
    let mut workspace = db_row
        .map(|(_, c, _)| c.clone())
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_default();
    let pinned = db_row.is_some_and(|(_, _, p)| *p);
    let mut git_branch = String::new();
    let mut msg_count = 0usize;

    if path.to_string_lossy().ends_with(".jsonl") {
        if let Ok((head, _)) = read_head_and_tail_lines(path, 28, 0) {
            for line in &head {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                match v.get("type").and_then(|x| x.as_str()) {
                    Some("session_meta") => {
                        if let Some(p) = v.get("payload") {
                            if workspace.is_empty() {
                                if let Some(c) = p.get("cwd").and_then(|x| x.as_str()) {
                                    workspace = c.to_string();
                                }
                            }
                            if git_branch.is_empty() {
                                if let Some(gb) = p.pointer("/git/branch").and_then(|x| x.as_str())
                                {
                                    git_branch = gb.to_string();
                                }
                            }
                        }
                    }
                    Some("event_msg") => {
                        if v.pointer("/payload/type").and_then(|x| x.as_str())
                            == Some("user_message")
                        {
                            msg_count += 1;
                            if title.is_empty() {
                                if let Some(m) =
                                    v.pointer("/payload/message").and_then(|x| x.as_str())
                                {
                                    title = truncate_title(m, 90);
                                }
                            }
                        }
                    }
                    Some("response_item") => {
                        if v.pointer("/payload/role").and_then(|x| x.as_str()) == Some("user") {
                            msg_count += 1;
                            if title.is_empty() {
                                if let Some(txt) =
                                    extract_quick_text(v.pointer("/payload/content"))
                                {
                                    title = truncate_title(&txt, 90);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if title.trim().is_empty() {
        title = format!("codex/{}", &session_id[..session_id.len().min(12)]);
    }

    Some(UnifiedSessionRow {
        session_id: session_id.to_string(),
        provider: "codex".to_string(),
        origin,
        title,
        custom_name: None,
        workspace,
        git_branch,
        message_count: msg_count,
        size_bytes,
        updated_at,
        pinned,
        encrypted: false,
        in_cloud: false,
        vault_id: None,
        source_path: Some(path.to_path_buf()),
    })
}

fn collect_grok_sessions(
    sessions_root: &Path,
    origin: &SessionOrigin,
    cloud_ids: &std::collections::HashMap<(String, String), String>,
    labels: &SessionLabelsStore,
    out: &mut Vec<UnifiedSessionRow>,
) {
    let Ok(groups) = fs::read_dir(sessions_root) else {
        return;
    };
    for grp in groups.flatten() {
        let gmeta = match fs::symlink_metadata(grp.path()) {
            Ok(m) if !m.file_type().is_symlink() && m.is_dir() => m,
            _ => continue,
        };
        let _ = gmeta;
        let Ok(sessions) = fs::read_dir(grp.path()) else {
            continue;
        };
        for sess in sessions.flatten() {
            let sdir = sess.path();
            if fs::symlink_metadata(&sdir)
                .map(|m| m.file_type().is_symlink() || !m.is_dir())
                .unwrap_or(true)
            {
                continue;
            }
            let updates = sdir.join("updates.jsonl");
            let meta = match fs::symlink_metadata(&updates) {
                Ok(m) if !m.file_type().is_symlink() && m.is_file() => m,
                _ => continue,
            };
            let Some(sid) = sdir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let mut title = format!("grok/{}", &sid[..sid.len().min(12)]);
            let mut workspace = String::new();
            let mut msg_count = 0usize;
            if let Ok(raw) = fs::read_to_string(sdir.join("summary.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
                        if !t.trim().is_empty() {
                            title = t.to_string();
                        }
                    }
                    if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                        workspace = c.to_string();
                    }
                    if let Some(n) = v.get("message_count").and_then(|x| x.as_u64()) {
                        msg_count = n as usize;
                    }
                }
            }
            let mut row = UnifiedSessionRow {
                session_id: sid.to_string(),
                provider: "grok".to_string(),
                origin: origin.clone(),
                title,
                custom_name: None,
                workspace,
                git_branch: String::new(),
                message_count: msg_count,
                size_bytes: meta.len(),
                updated_at: file_mtime_utc(&meta),
                pinned: false,
                encrypted: false,
                in_cloud: false,
                vault_id: None,
                source_path: Some(updates),
            };
            apply_cloud_and_labels(&mut row, cloud_ids, labels);
            out.push(row);
        }
    }
}

fn collect_agy_sessions(
    agy_dir: &Path,
    origin: &SessionOrigin,
    cloud_ids: &std::collections::HashMap<(String, String), String>,
    labels: &SessionLabelsStore,
    out: &mut Vec<UnifiedSessionRow>,
) {
    let brain_dir = agy_dir.join("brain");
    let Ok(entries) = fs::read_dir(&brain_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if fs::symlink_metadata(entry.path())
            .map(|m| m.file_type().is_symlink() || !m.is_dir())
            .unwrap_or(true)
        {
            continue;
        }
        let Some(sid) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let transcript = entry
            .path()
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let Ok(meta) = fs::symlink_metadata(&transcript) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let mut title = format!("agy/{}", &sid[..sid.len().min(12)]);
        if let Ok((head, _)) = read_head_and_tail_lines(&transcript, 12, 0) {
            for line in head {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if v.get("source").and_then(|x| x.as_str()) == Some("USER_EXPLICIT") {
                        if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                            let stripped = c
                                .replace("<USER_REQUEST>", "")
                                .replace("</USER_REQUEST>", "");
                            let trimmed = stripped.trim();
                            if !trimmed.is_empty() {
                                title = truncate_title(trimmed, 90);
                                break;
                            }
                        }
                    }
                }
            }
        }
        let mut row = UnifiedSessionRow {
            session_id: sid,
            provider: "antigravity".to_string(),
            origin: origin.clone(),
            title,
            custom_name: None,
            workspace: String::new(),
            git_branch: String::new(),
            message_count: 0,
            size_bytes: meta.len(),
            updated_at: file_mtime_utc(&meta),
            pinned: false,
            encrypted: false,
            in_cloud: false,
            vault_id: None,
            source_path: Some(transcript),
        };
        apply_cloud_and_labels(&mut row, cloud_ids, labels);
        out.push(row);
    }
}

fn read_head_and_tail_lines(
    path: &Path,
    head_limit: usize,
    tail_bytes: u64,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = Vec::new();
    {
        let reader = BufReader::new(&file);
        for line in reader.lines().take(head_limit) {
            if let Ok(l) = line {
                if !l.trim().is_empty() {
                    head.push(l);
                }
            }
        }
    }
    let mut tail = Vec::new();
    if tail_bytes > 0 && len > 0 {
        let start = len.saturating_sub(tail_bytes);
        if start > 0 {
            file.seek(SeekFrom::Start(start))?;
            let mut raw = String::new();
            let _ = file.read_to_string(&mut raw);
            // Skip partial first line after seek.
            for line in raw.lines().skip(1) {
                if !line.trim().is_empty() {
                    tail.push(line.to_string());
                }
            }
        }
    }
    Ok((head, tail))
}

fn extract_quick_text(val: Option<&serde_json::Value>) -> Option<String> {
    let v = val?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(t) = item
                .get("text")
                .or_else(|| item.get("input_text"))
                .and_then(|x| x.as_str())
            {
                if !t.trim().is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn file_mtime_utc(meta: &fs::Metadata) -> DateTime<Utc> {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

pub fn matches_provider_filter(provider: &str, filter: &str) -> bool {
    let p = provider.to_ascii_lowercase();
    let f = filter.trim().to_ascii_lowercase();
    match f.as_str() {
        "" | "all" => true,
        "cc" | "claude" | "claude-code" => p == "claude-code" || p == "claude",
        "cod" | "codex" => p == "codex",
        "agy" | "antigravity" => p == "antigravity",
        "grok" => p == "grok",
        "gmi" | "gemini" => p == "gemini",
        "opc" | "opencode" => p == "opencode",
        other => p == other,
    }
}

pub fn canonical_agent_name(target: &str) -> Result<&'static str> {
    match target.trim().to_ascii_lowercase().as_str() {
        "cc" | "claude" | "claude-code" => Ok("claude"),
        "cod" | "codex" => Ok("codex"),
        "agy" | "antigravity" => Ok("agy"),
        "grok" => Ok("grok"),
        other => bail!("unsupported launch target `{other}` (expected claude, codex, agy, or grok)"),
    }
}

pub fn provider_slug_for_agent(agent: &str) -> &'static str {
    match agent {
        "claude" | "claude-code" | "cc" => "claude-code",
        "codex" | "cod" => "codex",
        "agy" | "antigravity" => "antigravity",
        "grok" => "grok",
        _ => "codex",
    }
}

// ---------------------------------------------------------------------------
// Cage-Native Session Resolution, Conversion & Materialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PreparedResume {
    pub target_agent: &'static str,
    pub target_session_id: String,
    pub native_resume_args: Vec<String>,
    pub target_workspace: PathBuf,
    pub written_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Reads a session from a local file OR downloads & unpacks it from Berth Cloud / Local Store,
/// converts it to `target_agent` (or hydrates natively), rewrites workspace to `target_workspace`,
/// and writes directly into `target_cage_home` (refusing symlinks!).
pub fn materialize_session_into_cage(
    paths: &BerthPaths,
    row: &UnifiedSessionRow,
    target_agent: &'static str,
    target_workspace: &Path,
    target_cage_home: &AccountCageHome,
    visible_home_in_sandbox: &Path,
    decryption: &BundleDecryption,
) -> Result<PreparedResume> {
    let mut canonical = load_canonical_session(paths, row, decryption)?;
    canonical.workspace = Some(target_workspace.to_path_buf());

    let opts = WriteOptions { force: false };
    let written = match target_agent {
        "claude" => ClaudeCode.write_session_to_home(
            &target_cage_home.home_dir.join(".claude"),
            &canonical,
            &opts,
        )?,
        "codex" => Codex.write_session_to_home(
            &target_cage_home.home_dir.join(".codex"),
            &visible_home_in_sandbox.join(".codex"),
            &canonical,
            &opts,
        )?,
        other => {
            let registry = ProviderRegistry::default_registry();
            let slug = provider_slug_for_agent(other);
            let provider = registry
                .find_by_alias(slug)
                .ok_or_else(|| anyhow::anyhow!("unknown target provider `{slug}`"))?;
            provider.write_session(&canonical, &opts)?
        }
    };

    let native_resume_args = match target_agent {
        "claude" => vec!["--resume".to_string(), written.session_id.clone()],
        "codex" => vec!["resume".to_string(), written.session_id.clone()],
        "agy" => vec!["--conversation".to_string(), written.session_id.clone()],
        "grok" => vec!["--resume".to_string(), written.session_id.clone()],
        _ => vec![],
    };

    Ok(PreparedResume {
        target_agent,
        target_session_id: written.session_id,
        native_resume_args,
        target_workspace: target_workspace.to_path_buf(),
        written_paths: written.paths,
        warnings: written.warnings,
    })
}

pub fn load_canonical_session(
    paths: &BerthPaths,
    row: &UnifiedSessionRow,
    decryption: &BundleDecryption,
) -> Result<CanonicalSession> {
    if let Some(ref src) = row.source_path {
        if src.is_file() {
            return read_canonical_file(&row.provider, src);
        }
    }
    // Download from Berth Cloud Vault or read from local cache/store.
    let vid = row
        .vault_id
        .as_deref()
        .unwrap_or(&row.session_id);
    let unpacked: UnpackedBundle = match &row.origin {
        SessionOrigin::CloudVault(_) => berth_vault_download(paths, vid, decryption)?.1,
        SessionOrigin::LocalStore(store_label) => {
            let mut found = None;
            for (label, dir) in crate::vault_sync::all_local_cache_roots(paths) {
                if label == *store_label {
                    if let Ok((_, u)) = local_dir_get_bundle(&dir, vid, decryption) {
                        found = Some(u);
                        break;
                    }
                }
            }
            if let Some(u) = found {
                u
            } else {
                // Fallback scan across all local store caches.
                let mut any = None;
                for (_, dir) in crate::vault_sync::all_local_cache_roots(paths) {
                    if let Ok((_, u)) = local_dir_get_bundle(&dir, vid, decryption) {
                        any = Some(u);
                        break;
                    }
                }
                any.ok_or_else(|| anyhow::anyhow!("local vault bundle `{vid}` not found"))?
            }
        }
        _ => berth_vault_download(paths, vid, decryption)?.1,
    };

    let tmp = tempfile::Builder::new().prefix("berth-unpack-").tempdir()?;
    let primary = unpacked
        .manifest
        .files
        .iter()
        .find(|f| f.role == "transcript")
        .or_else(|| unpacked.manifest.files.first())
        .context("unpacked bundle contains no files")?;
    let bytes = unpacked
        .files
        .get(&primary.rel_path)
        .context("primary file bytes missing from unpacked bundle")?;
    let fname = Path::new(&primary.rel_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session.jsonl");
    let staged_file = tmp.path().join(fname);
    fs::write(&staged_file, bytes)?;
    let mut sess = read_canonical_file(&unpacked.manifest.provider, &staged_file)?;
    if sess.title.is_none() && !unpacked.manifest.title.is_empty() {
        sess.title = Some(unpacked.manifest.title);
    }
    Ok(sess)
}

fn read_canonical_file(provider_slug: &str, path: &Path) -> Result<CanonicalSession> {
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        anyhow::bail!("refusing to read symlinked session file {}", path.display());
    }
    // Support zstd-compressed Codex rollouts transparently with strict anti-zip-bomb cap!
    if provider_slug == "codex" && path.to_string_lossy().ends_with(".jsonl.zst") {
        let compressed = crate::vault_sync::read_regular_file_no_symlink(path)?;
        let decoded = crate::bundle::decompress_zstd_bounded(
            &compressed,
            crate::bundle::MAX_BUNDLE_UNCOMPRESSED_BYTES,
        )
        .with_context(|| format!("bounded decompress zstd rollout {}", path.display()))?;
        let tmp = tempfile::Builder::new().prefix("berth-zst-").tempdir()?;
        let out = tmp.path().join("rollout.jsonl");
        fs::write(&out, decoded)?;
        return Codex.read_session(&out);
    }
    let registry = ProviderRegistry::default_registry();
    let provider = registry
        .find_by_alias(provider_slug)
        .ok_or_else(|| anyhow::anyhow!("unsupported provider `{provider_slug}`"))?;
    provider.read_session(path)
}

/// Produces a compact preview of the last few conversational turns for the `bh ls` preview pane.
pub fn preview_session_tail(row: &UnifiedSessionRow, max_turns: usize) -> Vec<String> {
    let mut out = vec![
        format!("Title:     {}", row.display_title()),
        format!(
            "Provider:  {}  |  Origin: {}  |  Size: {}",
            row.provider,
            row.origin.short_tag(),
            format_bytes_human(row.size_bytes as i64)
        ),
        format!("ID:        {}", row.session_id),
        format!(
            "Workspace: {}",
            if row.workspace.is_empty() { "-" } else { &row.workspace }
        ),
    ];
    if !row.git_branch.is_empty() {
        out.push(format!("Branch:    {}", row.git_branch));
    }
    if let Some(ref vid) = row.vault_id {
        out.push(format!(
            "Cloud:     {} {}",
            vid,
            if row.encrypted { "(E2EE)" } else { "(redacted)" }
        ));
    }
    out.push(String::new());
    if let Some(ref p) = row.source_path {
        if let Ok(sess) = read_canonical_file(&row.provider, p) {
            let start = sess.messages.len().saturating_sub(max_turns);
            for msg in &sess.messages[start..] {
                out.push(format_preview_turn(msg));
            }
        }
    } else if row.encrypted {
        out.push("[Encrypted in Cloud Vault — press Enter or `c` to decrypt & resume]".into());
    } else {
        out.push("[Stored in Cloud Vault — press Enter or `c` to download & resume]".into());
    }
    out
}

fn format_preview_turn(msg: &CanonicalMessage) -> String {
    let role = match msg.role {
        MessageRole::User => "USER",
        MessageRole::Assistant => "ASST",
        MessageRole::Tool => "TOOL",
        MessageRole::System => "SYS ",
        MessageRole::Other(_) => "MSG ",
    };
    let snippet = sanitize_terminal_text(&truncate_title(&msg.content, 110));
    format!("  [{role}] {snippet}")
}

// ---------------------------------------------------------------------------
// Pure-Rust Cross-Platform Interactive TUI (`bh ls`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum TuiAction {
    Quit,
    Resume {
        row: UnifiedSessionRow,
        target_agent: &'static str,
    },
}

fn restore_terminal_best_effort() {
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
}

/// Runs the cross-platform `bh ls` command:
/// - Non-TTY or `--json`: prints deterministic table or JSON immediately.
/// - Interactive TTY: launches pure-Rust `crossterm` TUI with live fuzzy search, agent filter,
///   preview pane, one-key Berth Cloud Vault upload (`u`), rename (`n`), pin (`p`), delete (`d`),
///   native resume (`Enter`), and cross-agent convert & resume (`c`).
pub fn run_ls_command(
    paths: &BerthPaths,
    all_workspaces: bool,
    workspace_dir: Option<&Path>,
    provider_filter: Option<&str>,
    query_filter: Option<&str>,
    cloud_only: bool,
    json_mode: bool,
) -> Result<TuiAction> {
    let cwd = workspace_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cwd_key = normalize_workspace_key(&cwd.to_string_lossy());

    let discovery = discover_unified_sessions(paths, true, provider_filter);
    let initial_rows: Vec<UnifiedSessionRow> = discovery
        .rows
        .into_iter()
        .filter(|r| {
            if cloud_only && !r.in_cloud && !r.origin.is_cloud() {
                return false;
            }
            if let Some(q) = query_filter {
                if !row_matches_query(r, q) {
                    return false;
                }
            }
            if !all_workspaces && !r.workspace.is_empty() {
                normalize_workspace_key(&r.workspace) == cwd_key
            } else {
                true
            }
        })
        .collect();

    if json_mode {
        let out = serde_json::json!({
            "workspace": cwd.display().to_string(),
            "all_workspaces": all_workspaces,
            "cloud_online": discovery.cloud_online,
            "quota": discovery.quota,
            "sessions": initial_rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(TuiAction::Quit);
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print_plain_table(&initial_rows, discovery.quota.as_ref(), all_workspaces, &cwd);
        return Ok(TuiAction::Quit);
    }

    run_interactive_tui(
        paths,
        cwd,
        cwd_key,
        all_workspaces,
        cloud_only,
        provider_filter.map(ToOwned::to_owned),
        query_filter.unwrap_or("").to_string(),
    )
}

fn print_plain_table(
    rows: &[UnifiedSessionRow],
    quota: Option<&BerthQuotaInfo>,
    all_workspaces: bool,
    cwd: &Path,
) {
    if let Some(q) = quota {
        println!(
            "Berth Vault: {}  ({} snapshots)",
            render_quota_bar(q.used_bytes, q.quota_bytes, 20),
            q.item_count
        );
    }
    println!(
        "Scope: {}  ({} sessions)",
        if all_workspaces {
            "all workspaces".to_string()
        } else {
            cwd.display().to_string()
        },
        rows.len()
    );
    if rows.is_empty() {
        println!("  (no matching sessions; pass `-a` to list all workspaces)");
        return;
    }
    println!(
        "{:<4} {:<7} {:<12} {:<5} {:<16} {:<10} {}",
        "#", "AGENT", "ORIGIN", "CLOUD", "UPDATED", "SIZE", "TITLE"
    );
    for (idx, r) in rows.iter().enumerate() {
        let local_ts: DateTime<Local> = r.updated_at.into();
        let pin = if r.pinned { "*" } else { " " };
        let cloud = if r.encrypted {
            "LOCK"
        } else if r.in_cloud || r.origin.is_cloud() {
            "SYNC"
        } else {
            "-"
        };
        println!(
            "{:<4} {:<7} {:<12} {:<5} {:<16} {:<10} {}{}",
            idx + 1,
            r.short_provider(),
            truncate_title(&r.origin.short_tag(), 12),
            cloud,
            local_ts.format("%Y-%m-%d %H:%M"),
            format_bytes_human(r.size_bytes as i64),
            pin,
            truncate_title(&r.display_title(), 64),
        );
    }
}

fn row_matches_query(r: &UnifiedSessionRow, query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {} {}",
        r.display_title(),
        r.provider,
        r.session_id,
        r.workspace,
        r.git_branch,
        r.origin.short_tag()
    )
    .to_ascii_lowercase();
    q.split_whitespace().all(|tok| hay.contains(tok))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiInputMode {
    Normal,
    Search,
    Rename,
    ConvertTarget,
    ConfirmDelete,
}

fn run_interactive_tui(
    paths: &BerthPaths,
    cwd: PathBuf,
    cwd_key: String,
    mut all_workspaces: bool,
    mut cloud_only: bool,
    initial_provider: Option<String>,
    mut search_query: String,
) -> Result<TuiAction> {
    // Install panic hook BEFORE enabling raw mode (critical because Cargo release has panic="abort"!).
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_best_effort();
        prev_hook(info);
    }));

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let mut discovery = discover_unified_sessions(paths, true, None);
    let providers_cycle = ["all", "claude", "codex", "agy", "grok"];
    let mut provider_idx = initial_provider
        .as_deref()
        .and_then(|p| providers_cycle.iter().position(|x| matches_provider_filter(p, x)))
        .unwrap_or(0);
    let mut selected: usize = 0;
    let mut show_preview = false;
    let mut mode = TuiInputMode::Normal;
    let mut input_buf = String::new();
    let mut status_msg = discovery
        .cloud_error
        .clone()
        .map(|e| format!("Cloud offline: {e}"))
        .unwrap_or_else(|| "Ready — / search · f agent · a scope · u upload · c convert · i preview · Enter resume".into());

    let berth_cfg: Option<BerthConfig> = load_config(paths).ok().flatten();
    let result: Result<TuiAction> = loop {
        let active_provider = providers_cycle[provider_idx];
        let filtered: Vec<UnifiedSessionRow> = discovery
            .rows
            .iter()
            .filter(|r| {
                if active_provider != "all" && !matches_provider_filter(&r.provider, active_provider) {
                    return false;
                }
                if cloud_only && !r.in_cloud && !r.origin.is_cloud() {
                    return false;
                }
                if !all_workspaces && !r.workspace.is_empty() && normalize_workspace_key(&r.workspace) != cwd_key {
                    return false;
                }
                row_matches_query(r, &search_query)
            })
            .cloned()
            .collect();

        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }

        if let Err(e) = render_tui_frame(
            &mut stdout,
            &filtered,
            selected,
            discovery.quota.as_ref(),
            active_provider,
            all_workspaces,
            cloud_only,
            &cwd,
            &search_query,
            show_preview,
            &mode,
            &input_buf,
            &status_msg,
        ) {
            break Err(e);
        }

        if !event::poll(Duration::from_millis(250)).unwrap_or(false) {
            continue;
        }
        let Ok(ev) = event::read() else {
            continue;
        };
        let Event::Key(KeyEvent { code, modifiers, .. }) = ev else {
            continue;
        };
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            break Ok(TuiAction::Quit);
        }

        match mode {
            TuiInputMode::Search => match code {
                KeyCode::Esc | KeyCode::Enter => mode = TuiInputMode::Normal,
                KeyCode::Backspace => {
                    search_query.pop();
                    selected = 0;
                }
                KeyCode::Char(ch) => {
                    search_query.push(ch);
                    selected = 0;
                }
                _ => {}
            },
            TuiInputMode::Rename => match code {
                KeyCode::Esc => {
                    mode = TuiInputMode::Normal;
                    input_buf.clear();
                }
                KeyCode::Backspace => {
                    input_buf.pop();
                }
                KeyCode::Char(ch) => input_buf.push(ch),
                KeyCode::Enter => {
                    if let Some(row) = filtered.get(selected) {
                        let new_name = input_buf.trim().to_string();
                        let mut lbls = load_session_labels(paths);
                        let entry = lbls.labels.entry(row.label_key()).or_default();
                        entry.name = if new_name.is_empty() { None } else { Some(new_name.clone()) };
                        let _ = save_session_labels(paths, &lbls);
                        if let Some(ref vid) = row.vault_id {
                            if !row.encrypted && !new_name.is_empty() {
                                let _ = berth_vault_patch(paths, vid, Some(&new_name), None, None);
                            }
                        }
                        discovery = discover_unified_sessions(paths, true, None);
                        status_msg = format!("Renamed session {}", row.session_id);
                    }
                    input_buf.clear();
                    mode = TuiInputMode::Normal;
                }
                _ => {}
            },
            TuiInputMode::ConvertTarget => match code {
                KeyCode::Esc => mode = TuiInputMode::Normal,
                KeyCode::Char('1') | KeyCode::Char('c') => {
                    if let Some(row) = filtered.get(selected).cloned() {
                        break Ok(TuiAction::Resume { row, target_agent: "claude" });
                    }
                }
                KeyCode::Char('2') | KeyCode::Char('x') => {
                    if let Some(row) = filtered.get(selected).cloned() {
                        break Ok(TuiAction::Resume { row, target_agent: "codex" });
                    }
                }
                KeyCode::Char('3') | KeyCode::Char('a') => {
                    if let Some(row) = filtered.get(selected).cloned() {
                        break Ok(TuiAction::Resume { row, target_agent: "agy" });
                    }
                }
                KeyCode::Char('4') | KeyCode::Char('g') => {
                    if let Some(row) = filtered.get(selected).cloned() {
                        break Ok(TuiAction::Resume { row, target_agent: "grok" });
                    }
                }
                _ => {}
            },
            TuiInputMode::ConfirmDelete => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(row) = filtered.get(selected) {
                        if let Some(ref vid) = row.vault_id {
                            let res = match &row.origin {
                                SessionOrigin::LocalStore(store_label) => {
                                    let dir = crate::vault_sync::all_local_cache_roots(paths)
                                        .into_iter()
                                        .find(|(lbl, _)| lbl == store_label)
                                        .map(|(_, d)| d)
                                        .unwrap_or_else(|| local_cache_root(paths));
                                    local_dir_delete(&dir, vid).map(|_| None)
                                }
                                _ => berth_vault_delete(paths, vid).map(Some),
                            };
                            match res {
                                Ok(new_q) => {
                                    if let Some(q) = new_q {
                                        discovery.quota = Some(q);
                                    }
                                    discovery = discover_unified_sessions(paths, true, None);
                                    status_msg = format!("Tombstoned cloud session {vid}");
                                }
                                Err(e) => status_msg = format!("Delete failed: {e}"),
                            }
                        } else {
                            status_msg = "Selected session is local-only (not in cloud vault)".into();
                        }
                    }
                    mode = TuiInputMode::Normal;
                }
                _ => {
                    mode = TuiInputMode::Normal;
                    status_msg = "Delete cancelled".into();
                }
            },
            TuiInputMode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Esc => break Ok(TuiAction::Quit),
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < filtered.len() {
                        selected += 1;
                    }
                }
                KeyCode::Char('/') => mode = TuiInputMode::Search,
                KeyCode::Char('f') => {
                    provider_idx = (provider_idx + 1) % providers_cycle.len();
                    selected = 0;
                }
                KeyCode::Char('a') => {
                    all_workspaces = !all_workspaces;
                    selected = 0;
                }
                KeyCode::Char('v') => {
                    cloud_only = !cloud_only;
                    selected = 0;
                }
                KeyCode::Char('i') | KeyCode::Char(' ') => {
                    show_preview = !show_preview;
                }
                KeyCode::Char('r') => {
                    discovery = discover_unified_sessions(paths, true, None);
                    status_msg = "Refreshed local + cage + cloud sessions".into();
                }
                KeyCode::Char('n') => {
                    if let Some(row) = filtered.get(selected) {
                        input_buf = row.custom_name.clone().unwrap_or_default();
                        mode = TuiInputMode::Rename;
                    }
                }
                KeyCode::Char('p') => {
                    if let Some(row) = filtered.get(selected) {
                        let new_pin = !row.pinned;
                        let mut lbls = load_session_labels(paths);
                        lbls.labels.entry(row.label_key()).or_default().pinned = new_pin;
                        let _ = save_session_labels(paths, &lbls);
                        if let Some(ref vid) = row.vault_id {
                            let _ = berth_vault_patch(paths, vid, None, None, Some(new_pin));
                        }
                        discovery = discover_unified_sessions(paths, true, None);
                        status_msg = if new_pin { "Pinned session".into() } else { "Unpinned session".into() };
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(row) = filtered.get(selected) {
                        if let Some(ref src) = row.source_path {
                            let reg = load_store_registry(paths);
                            let encrypt = reg
                                .stores
                                .get(&reg.active_store)
                                .is_some_and(|s| s.encrypt_by_default);
                            match save_session_file_to_store(
                                paths,
                                berth_cfg.as_ref(),
                                &row.provider,
                                &row.session_id,
                                &row.origin.short_tag(),
                                src,
                                &row.display_title(),
                                "",
                                &row.workspace,
                                &row.git_branch,
                                row.message_count,
                                encrypt,
                                None,
                                false,
                                None,
                                row.pinned,
                            ) {
                                Ok((saved, new_q, built)) => {
                                    if let Some(q) = new_q {
                                        discovery.quota = Some(q);
                                    }
                                    discovery = discover_unified_sessions(paths, true, None);
                                    status_msg = format!(
                                        "Uploaded {} ({}, {} secrets scrubbed)",
                                        saved.vault_id,
                                        format_bytes_human(saved.stored_bytes),
                                        built.manifest.redaction.total_redactions
                                    );
                                }
                                Err(e) => status_msg = format!("Upload failed: {e}"),
                            }
                        } else {
                            status_msg = "Already stored in cloud vault".into();
                        }
                    }
                }
                KeyCode::Char('c') => {
                    if !filtered.is_empty() {
                        mode = TuiInputMode::ConvertTarget;
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(row) = filtered.get(selected) {
                        if row.vault_id.is_some() {
                            mode = TuiInputMode::ConfirmDelete;
                        } else {
                            status_msg = "Only cloud/store snapshots can be deleted with `d`".into();
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(row) = filtered.get(selected).cloned() {
                        let target_agent = canonical_agent_name(&row.provider).unwrap_or("claude");
                        break Ok(TuiAction::Resume { row, target_agent });
                    }
                }
                _ => {}
            },
        }
    };

    restore_terminal_best_effort();
    result
}

#[allow(clippy::too_many_arguments)]
fn render_tui_frame(
    stdout: &mut io::Stdout,
    rows: &[UnifiedSessionRow],
    selected: usize,
    quota: Option<&BerthQuotaInfo>,
    active_provider: &str,
    all_workspaces: bool,
    cloud_only: bool,
    cwd: &Path,
    search_query: &str,
    show_preview: bool,
    mode: &TuiInputMode,
    input_buf: &str,
    status_msg: &str,
) -> Result<()> {
    let (cols, term_rows) = terminal::size().unwrap_or((110, 30));
    let width = cols as usize;
    let height = term_rows as usize;
    execute!(stdout, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0))?;

    let scope = if all_workspaces {
        "ALL".to_string()
    } else {
        truncate_title(&cwd.display().to_string(), 32)
    };
    let quota_str = match quota {
        Some(q) => render_quota_bar(q.used_bytes, q.quota_bytes, 14),
        None => "local-only".to_string(),
    };
    write!(
        stdout,
        " Berth Sessions ({}) | Agent: {} | Scope: {} | View: {} | Vault: {}\r\n",
        rows.len(),
        active_provider,
        scope,
        if cloud_only { "CLOUD" } else { "ALL" },
        quota_str
    )?;
    write!(stdout, "{}\r\n", "-".repeat(width.min(140)))?;

    let footer_lines = if show_preview { 12 } else { 3 };
    let list_height = height.saturating_sub(footer_lines + 3).max(3);
    let scroll = if selected >= list_height {
        selected + 1 - list_height
    } else {
        0
    };

    if rows.is_empty() {
        write!(stdout, "  (No sessions match current filter — press `a` for all workspaces or `/` to clear search)\r\n")?;
    } else {
        for (idx, r) in rows.iter().enumerate().skip(scroll).take(list_height) {
            let marker = if idx == selected { ">" } else { " " };
            let pin = if r.pinned { "*" } else { " " };
            let cloud = if r.encrypted {
                "ENC"
            } else if r.in_cloud || r.origin.is_cloud() {
                "CLD"
            } else {
                "   "
            };
            let ts: DateTime<Local> = r.updated_at.into();
            let title_max = width.saturating_sub(54).max(16);
            let line = format!(
                "{marker}{pin} {:<6} {:<11} {:<3} {:<11} {:>8}  {}",
                r.short_provider(),
                truncate_title(&r.origin.short_tag(), 11),
                cloud,
                ts.format("%m-%d %H:%M"),
                format_bytes_human(r.size_bytes as i64),
                truncate_title(&r.display_title(), title_max),
            );
            write!(stdout, "{}\r\n", truncate_title(&line, width.saturating_sub(1)))?;
        }
    }

    if show_preview {
        write!(stdout, "{}\r\n", "-".repeat(width.min(140)))?;
        if let Some(row) = rows.get(selected) {
            for pline in preview_session_tail(row, 4).into_iter().take(9) {
                write!(stdout, " {}\r\n", truncate_title(&pline, width.saturating_sub(2)))?;
            }
        }
    }

    write!(stdout, "{}\r\n", "-".repeat(width.min(140)))?;
    match mode {
        TuiInputMode::Search => {
            write!(stdout, " Search: {search_query}_\r\n")?;
        }
        TuiInputMode::Rename => {
            write!(stdout, " Rename title (Enter save, Esc cancel): {input_buf}_\r\n")?;
        }
        TuiInputMode::ConvertTarget => {
            write!(
                stdout,
                " Convert & Resume in Sandbox -> [1] Claude  [2] Codex  [3] Antigravity  [4] Grok  [Esc] Cancel\r\n"
            )?;
        }
        TuiInputMode::ConfirmDelete => {
            write!(stdout, " Tombstone delete from Cloud Vault? [y] Yes / [n] Cancel\r\n")?;
        }
        TuiInputMode::Normal => {
            let search_hint = if search_query.is_empty() {
                String::new()
            } else {
                format!(" [filter: \"{search_query}\"]")
            };
            write!(stdout, " {}{}\r\n", truncate_title(status_msg, width.saturating_sub(2)), search_hint)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Resolves default decryption mode (passphrase if provided, else managed age identity file if present).
pub fn default_decryption_for(paths: &BerthPaths, passphrase: Option<&str>) -> BundleDecryption {
    if let Some(pass) = passphrase {
        return BundleDecryption::Passphrase(pass.to_string());
    }
    let id_path = default_age_identity_path(paths);
    if id_path.is_file() {
        BundleDecryption::IdentityFile(id_path)
    } else {
        BundleDecryption::None
    }
}

/// Helper for `bh store` display summary.
pub fn format_store_backend(backend: &StoreBackendConfig) -> String {
    match backend {
        StoreBackendConfig::Berth => "berth-cloud (1 GiB/user default quota)".to_string(),
        StoreBackendConfig::Local { path } => format!("local: {}", path.display()),
        StoreBackendConfig::Git { url, branch } => format!("git: {url} (branch={branch})"),
        StoreBackendConfig::Sftp {
            target,
            port,
            remote_dir,
            ..
        } => {
            let port_part = port.map(|p| format!(":{p}")).unwrap_or_default();
            format!("sftp: {target}{port_part}{remote_dir}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ansi_escapes_from_untrusted_titles() {
        let dirty = "\x1b[31mEvil\x1b[0m\tTitle\nLine2\x07";
        assert_eq!(sanitize_terminal_text(dirty), "Evil Title Line2");
    }

    #[test]
    fn fuzzy_search_matches_multi_token_queries() {
        let row = UnifiedSessionRow {
            session_id: "01a21713-575e".into(),
            provider: "codex".into(),
            origin: SessionOrigin::Cage("acct-7".into()),
            title: "Fix Berth Vault Quota".into(),
            custom_name: None,
            workspace: "/work/AgentOps-Hub".into(),
            git_branch: "main".into(),
            message_count: 12,
            size_bytes: 4096,
            updated_at: Utc::now(),
            pinned: false,
            encrypted: false,
            in_cloud: true,
            vault_id: Some("vlt_123".into()),
            source_path: None,
        };
        assert!(row_matches_query(&row, "codex vault"));
        assert!(row_matches_query(&row, "acct-7 quota"));
        assert!(!row_matches_query(&row, "claude vault"));
    }
}
