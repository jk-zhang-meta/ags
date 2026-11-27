//! Unified Pure-Rust 4-Backend Session Vault & Multi-Device Sync Engine.
//!
//! Replaces the legacy 13,258-line Bash checkpoint sync runtime across Linux, WSL2, macOS,
//! and native Windows with one immutable-snapshot + digest-bound-tombstone sync model:
//!
//! 1. **`berth`** (default when logged in): Berth Cloud Session Vault (`/api/v1/berth/vault/*`,
//!    1 GiB/user default quota, incremental `GET /changes?since=seq`, tombstones, admin overrides).
//! 2. **`local`**: Dedicated private directory store (no-clobber hard-link publication).
//! 3. **`git`**: GitHub / any Git remote (persistent bare mirror, hardened `git` flags blocking
//!    hooks/fsmonitor/`ext::`/`file://`, `GIT_TERMINAL_PROMPT=0`, fast-forward push + refetch).
//! 4. **`sftp` / `scp`**: Native OpenSSH `sftp -b -` (`C:\Windows\System32\OpenSSH\sftp.exe` on
//!    Windows), Berth-owned `UserKnownHostsFile`, `StrictHostKeyChecking=yes`, zero remote POSIX
//!    shell / `rclone` / `sshpass` requirement!

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::berth::{
    BerthConfig, BerthError, BerthPaths, load_config, resolve_vault_owner_token,
    vault_http_request, write_private_file_atomic,
};
use crate::bundle::{
    BuiltBundle, BundleDecryption, BundleEncryption, BundleInputFile, BundleManifest,
    UnpackedBundle, build_bundle, ensure_age_identity, sha256_hex, unpack_bundle,
};
use crate::redact::Redactor;

const MAX_VAULT_DOWNLOAD_BYTES: usize = 110 * 1024 * 1024;
const STORES_CONFIG_FILE: &str = "stores.json";
const LOCAL_INDEX_FILE: &str = "vault-index.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StoreBackendConfig {
    /// Built-in Berth Cloud Session Vault (1 GiB default per-user quota).
    Berth,
    /// Dedicated private local filesystem directory.
    Local { path: PathBuf },
    /// GitHub / Git remote repository with dedicated branch/ref.
    Git {
        url: String,
        #[serde(default = "default_git_branch")]
        branch: String,
    },
    /// OpenSSH SFTP (or `scp://` alias) remote directory with pinned known_hosts.
    Sftp {
        target: String,
        #[serde(default)]
        port: Option<u16>,
        remote_dir: String,
        #[serde(default)]
        identity_file: Option<PathBuf>,
        #[serde(default)]
        known_hosts_file: Option<PathBuf>,
    },
}

fn default_git_branch() -> String {
    "berth-vault".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedStore {
    pub name: String,
    pub backend: StoreBackendConfig,
    #[serde(default)]
    pub encrypt_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreRegistryConfig {
    pub schema_version: u32,
    pub active_store: String,
    pub stores: BTreeMap<String, NamedStore>,
}

impl Default for StoreRegistryConfig {
    fn default() -> Self {
        let mut stores = BTreeMap::new();
        stores.insert(
            "berth".to_string(),
            NamedStore {
                name: "berth".to_string(),
                backend: StoreBackendConfig::Berth,
                encrypt_by_default: false,
            },
        );
        Self {
            schema_version: 1,
            active_store: "berth".to_string(),
            stores,
        }
    }
}

/// Summary record for a session snapshot in ANY of the 4 backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultRecordSummary {
    pub vault_id: String,
    pub logical_id: String,
    pub revision: i32,
    pub provider: String,
    pub source_session_id: String,
    pub title: String,
    pub description: String,
    pub workspace_hint: String,
    pub git_repo: String,
    pub git_branch: String,
    pub tags: Vec<String>,
    pub message_count: i32,
    pub raw_bytes: i64,
    pub stored_bytes: i64,
    pub sha256: String,
    pub encoding: String,
    pub encrypted: bool,
    pub redacted: bool,
    pub redacted_count: i32,
    pub pinned: bool,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tombstone_sha256: Option<String>,
    #[serde(default)]
    pub change_seq: i64,
    #[serde(default)]
    pub sealed_meta_b64: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    Both,
    PushOnly,
    PullOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BerthQuotaInfo {
    pub user_id: i64,
    pub used_bytes: i64,
    pub reserved_bytes: i64,
    pub quota_bytes: i64,
    pub remaining_bytes: i64,
    pub max_item_bytes: i64,
    pub item_count: i64,
    pub has_custom_quota: bool,
    pub default_quota_bytes: i64,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoreIndexFile {
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_change_seq: i64,
    pub records: BTreeMap<String, VaultRecordSummary>,
    pub tombstones: BTreeMap<String, TombstoneEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstoneEntry {
    pub vault_id: String,
    pub logical_id: String,
    pub sha256: String,
    pub deleted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub store_name: String,
    pub pushed: usize,
    pub pulled: usize,
    pub tombstones_applied: usize,
    pub dry_run: bool,
}

/// Validates a 64-character lowercase hex SHA-256 digest before it may ever enter
/// any filesystem path, Git object path, or OpenSSH SFTP batch command.
pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Restricts store names to a single safe path component `[A-Za-z0-9_-]{1,64}`.
pub fn validate_store_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        bail!(
            "invalid store name `{name}`: must be 1-64 characters of [A-Za-z0-9_-] and not start with `-`"
        );
    }
    Ok(())
}

/// Validates EVERY digest in a deserialized remote or local `StoreIndexFile` BEFORE
/// any path or SFTP batch command is constructed (fail-closed defense against P0 batch injection).
pub fn validate_store_index(idx: &StoreIndexFile) -> Result<()> {
    for (vid, rec) in &idx.records {
        if vid.is_empty() || vid.bytes().any(|b| b < 0x20 || b == b'/' || b == b'\\') {
            bail!("invalid vault_id `{vid}` in store index");
        }
        if !is_valid_sha256_hex(&rec.sha256) {
            bail!(
                "invalid SHA-256 digest `{}` for record `{vid}` in store index",
                rec.sha256
            );
        }
    }
    for (sha, ts) in &idx.tombstones {
        if !is_valid_sha256_hex(sha) || !is_valid_sha256_hex(&ts.sha256) {
            bail!("invalid SHA-256 digest in store index tombstone `{sha}`");
        }
    }
    Ok(())
}

/// Reads a regular file while refusing symlinks (uses `O_NOFOLLOW` on Unix to prevent TOCTOU).
pub fn read_regular_file_no_symlink(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("refusing to read symlinked file {}", path.display());
    }
    if !meta.is_file() {
        bail!("refusing to read non-regular file {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("open regular file {}", path.display()))?;
        let mut buf = Vec::with_capacity(meta.len().min(64 * 1024 * 1024) as usize);
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }
    #[cfg(not(unix))]
    {
        fs::read(path).with_context(|| format!("read {}", path.display()))
    }
}

/// Loads `stores.json`, failing closed if the file exists but is unreadable or corrupted.
pub fn load_store_registry_checked(paths: &BerthPaths) -> Result<StoreRegistryConfig> {
    let file = paths.state_dir.join(STORES_CONFIG_FILE);
    if !file.exists() {
        return Ok(StoreRegistryConfig::default());
    }
    let raw = fs::read_to_string(&file)
        .with_context(|| format!("read store registry {}", file.display()))?;
    let mut cfg: StoreRegistryConfig = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parse store registry {} (refusing to silently overwrite corrupted registry)",
            file.display()
        )
    })?;
    cfg.stores
        .entry("berth".to_string())
        .or_insert_with(|| NamedStore {
            name: "berth".to_string(),
            backend: StoreBackendConfig::Berth,
            encrypt_by_default: false,
        });
    Ok(cfg)
}

pub fn load_store_registry(paths: &BerthPaths) -> StoreRegistryConfig {
    load_store_registry_checked(paths).unwrap_or_default()
}

pub fn save_store_registry(paths: &BerthPaths, cfg: &StoreRegistryConfig) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let file = paths.state_dir.join(STORES_CONFIG_FILE);
    let data = serde_json::to_vec_pretty(cfg)?;
    write_private_file_atomic(&file, &data).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

pub fn local_cache_root(paths: &BerthPaths) -> PathBuf {
    paths.state_dir.join("vault-cache")
}

/// Returns the isolated per-store cache root so saves to one store never leak into another store's sync.
pub fn store_cache_root(paths: &BerthPaths, store_name: &str) -> Result<PathBuf> {
    validate_store_name(store_name)?;
    if store_name == "berth" {
        let issuer_tag = crate::berth::load_config(paths)
            .ok()
            .flatten()
            .map(|c| {
                let h = sha256_hex(c.central_url.trim().as_bytes());
                format!("berth_{}", &h[..12])
            })
            .unwrap_or_else(|| "berth".to_string());
        Ok(paths.state_dir.join("vault-cache").join("stores").join(issuer_tag))
    } else {
        Ok(paths.state_dir.join("vault-cache").join("stores").join(store_name))
    }
}

/// Lists all per-store cache roots (plus legacy shared cache root if present) for read-only discovery.
pub fn all_local_cache_roots(paths: &BerthPaths) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let stores_dir = paths.state_dir.join("vault-cache").join("stores");
    if let Ok(entries) = fs::read_dir(&stores_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    out.push((name.to_string(), p));
                }
            }
        }
    }
    let legacy = local_cache_root(paths);
    if legacy.join(LOCAL_INDEX_FILE).is_file() {
        out.push(("local-cache".to_string(), legacy));
    }
    out
}

pub fn default_age_identity_path(paths: &BerthPaths) -> PathBuf {
    paths.config_file
        .parent()
        .unwrap_or(&paths.state_dir)
        .join("identity.agekey")
}

pub fn format_bytes_human(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

pub fn render_quota_bar(used: i64, quota: i64, width: usize) -> String {
    if quota <= 0 {
        return format!("[{}] disabled", "-".repeat(width));
    }
    let ratio = (used.max(0) as f64 / quota as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "[{}{}] {:.1}% ({} / {})",
        "#".repeat(filled),
        ".".repeat(empty),
        ratio * 100.0,
        format_bytes_human(used),
        format_bytes_human(quota)
    )
}

/// Validates Git URLs and branch names against option injection (`-`), remote-helper
/// syntax (`helper::...`), whitespace, and unsafe schemes (`file://`, `ext::`).
pub fn validate_git_remote_args(url: &str, branch: &str) -> Result<()> {
    let url = url.trim();
    let branch = branch.trim();
    if url.is_empty()
        || url.starts_with('-')
        || url.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20)
    {
        bail!("invalid git remote URL `{url}`");
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("ext:") || lower.starts_with("file:") || url.contains("::") {
        bail!(
            "unsupported git remote URL `{url}`: only `https://`, `ssh://`, or `git@host:org/repo.git` are permitted (file:// and remote helpers like ext:: are blocked)"
        );
    }
    let is_scp_style = if let Some((lhs, rhs)) = url.split_once(':') {
        if let Some((user, host)) = lhs.split_once('@') {
            !user.is_empty()
                && !user.starts_with('-')
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                && !host.is_empty()
                && !host.starts_with('-')
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
                && !rhs.is_empty()
                && !rhs.starts_with('-')
                && !rhs.starts_with(':')
        } else {
            false
        }
    } else {
        false
    };
    let allowed = url.starts_with("https://") || url.starts_with("ssh://") || is_scp_style;
    if !allowed {
        bail!(
            "unsupported git remote URL `{url}`: only `https://`, `ssh://`, or `git@host:org/repo.git` are permitted (file:// and ext:: are blocked)"
        );
    }
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20)
    {
        bail!("invalid git branch name `{branch}`");
    }
    Ok(())
}

/// Parses an `sftp://user@host[:port]/abs/path` or `scp://user@host[:port]/abs/path` URL.
pub fn parse_sftp_url(raw: &str) -> Result<(String, Option<u16>, String)> {
    let trimmed = raw.trim();
    if trimmed.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20 || b == b'"' || b == b'\'' || b == b'\\') {
        bail!("SFTP/SCP URL must not contain whitespace, quotes, or control characters");
    }
    let rest = trimmed
        .strip_prefix("sftp://")
        .or_else(|| trimmed.strip_prefix("scp://"))
        .ok_or_else(|| anyhow::anyhow!("remote URL must start with `sftp://` or `scp://`"))?;
    let Some((authority, path_part)) = rest.split_once('/') else {
        bail!("SFTP/SCP URL must include an absolute remote directory path, e.g. sftp://user@host/srv/berth-vault");
    };
    if authority.is_empty() || authority.starts_with('-') {
        bail!("invalid SFTP host authority `{authority}`");
    }
    let remote_dir = format!("/{}", path_part.trim_start_matches('/'));
    if remote_dir.split('/').any(|seg| seg == "..") {
        bail!("SFTP remote directory must not contain `..` path traversal segments");
    }
    if authority.contains('@') {
        let (user_part, host_port) = authority.split_once('@').unwrap();
        if user_part.is_empty() || user_part.starts_with('-') {
            bail!("invalid SFTP username `{user_part}`");
        }
        if user_part.contains(':') {
            bail!("plaintext passwords in SFTP/SCP URLs are forbidden; use SSH keys or ssh-agent");
        }
        let (host, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
            let parsed_port: u16 = p.parse().context("invalid SFTP port")?;
            (h, Some(parsed_port))
        } else {
            (host_port, None)
        };
        if host.is_empty() || host.starts_with('-') {
            bail!("invalid SFTP hostname `{host}`");
        }
        return Ok((format!("{user_part}@{host}"), port, remote_dir));
    }
    let (host, port) = if let Some((h, p)) = authority.rsplit_once(':') {
        let parsed_port: u16 = p.parse().context("invalid SFTP port")?;
        (h, Some(parsed_port))
    } else {
        (authority, None)
    };
    if host.is_empty() || host.starts_with('-') {
        bail!("invalid SFTP hostname `{host}`");
    }
    Ok((host.to_string(), port, remote_dir))
}

// ---------------------------------------------------------------------------
// Backend 1: Berth Cloud Session Vault (`berth`)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BerthApiEnvelope<T> {
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

fn load_berth_config(paths: &BerthPaths) -> Result<BerthConfig, BerthError> {
    load_config(paths)?.ok_or(BerthError::NotConfigured)
}

pub fn berth_vault_quota(paths: &BerthPaths) -> Result<BerthQuotaInfo> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "GET",
        "/api/v1/berth/vault/quota",
        &[],
        None,
        1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault quota failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    let env: BerthApiEnvelope<BerthQuotaInfo> = serde_json::from_slice(&resp.body)?;
    if env.code != 0 {
        bail!("Berth Cloud Vault error: {}", env.message);
    }
    env.data.context("missing quota data in response")
}

pub fn berth_vault_list(
    paths: &BerthPaths,
    provider: Option<&str>,
    query: Option<&str>,
) -> Result<(Vec<VaultRecordSummary>, BerthQuotaInfo)> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut qs = vec!["limit=200".to_string()];
    if let Some(p) = provider.filter(|s| !s.trim().is_empty()) {
        qs.push(format!("provider={}", urlencoding::encode(p.trim())));
    }
    if let Some(q) = query.filter(|s| !s.trim().is_empty()) {
        qs.push(format!("q={}", urlencoding::encode(q.trim())));
    }
    let path = format!("/api/v1/berth/vault/sessions?{}", qs.join("&"));
    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "GET",
        &path,
        &[],
        None,
        8 * 1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault list failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    #[derive(Deserialize)]
    struct ListPayload {
        items: Vec<VaultRecordSummary>,
        quota: BerthQuotaInfo,
    }
    let env: BerthApiEnvelope<ListPayload> = serde_json::from_slice(&resp.body)?;
    if env.code != 0 {
        bail!("Berth Cloud Vault list error: {}", env.message);
    }
    let data = env.data.context("missing list payload")?;
    Ok((data.items, data.quota))
}

pub fn berth_vault_upload(
    paths: &BerthPaths,
    bundle: &BuiltBundle,
    pinned: bool,
    redacted: bool,
) -> Result<(VaultRecordSummary, BerthQuotaInfo)> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;

    let sealed_meta_b64 = bundle
        .sealed_meta
        .as_ref()
        .map(|b| base64::engine::general_purpose::STANDARD.encode(b));

    let meta_json = if bundle.encrypted {
        serde_json::json!({
            "logical_id": bundle.manifest.logical_id,
            "provider": bundle.manifest.provider,
            "sealed_meta_b64": sealed_meta_b64,
            "message_count": bundle.manifest.message_count,
            "raw_bytes": bundle.raw_bytes,
            "encoding": bundle.encoding,
            "encrypted": true,
            "redacted": redacted,
            "redacted_count": bundle.manifest.redaction.total_redactions,
            "schema_version": bundle.manifest.schema_version,
            "pinned": pinned,
        })
    } else {
        serde_json::json!({
            "logical_id": bundle.manifest.logical_id,
            "provider": bundle.manifest.provider,
            "source_session_id": bundle.manifest.source_session_id,
            "title": bundle.manifest.title,
            "description": bundle.manifest.description,
            "workspace_hint": bundle.manifest.workspace_hint,
            "git_repo": bundle.manifest.git_repo,
            "git_branch": bundle.manifest.git_branch,
            "tags": bundle.manifest.tags,
            "message_count": bundle.manifest.message_count,
            "raw_bytes": bundle.raw_bytes,
            "encoding": bundle.encoding,
            "encrypted": false,
            "redacted": redacted,
            "redacted_count": bundle.manifest.redaction.total_redactions,
            "schema_version": bundle.manifest.schema_version,
            "pinned": pinned,
        })
    };

    let meta_b64 = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&meta_json)?);
    let headers = [
        ("X-Berth-Vault-Meta", meta_b64),
        ("X-Berth-Vault-SHA256", bundle.sha256.clone()),
        ("Content-Type", "application/octet-stream".to_string()),
    ];

    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "PUT",
        "/api/v1/berth/vault/sessions",
        &headers,
        Some(&bundle.bytes),
        4 * 1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if resp.status == 413 {
        let body_str = String::from_utf8_lossy(&resp.body);
        bail!("Berth Cloud Vault quota exceeded (HTTP 413): {body_str}");
    }
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault upload failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    #[derive(Deserialize)]
    struct UploadPayload {
        item: VaultRecordSummary,
        quota: BerthQuotaInfo,
    }
    let env: BerthApiEnvelope<UploadPayload> = serde_json::from_slice(&resp.body)?;
    if env.code != 0 {
        bail!("Berth Cloud Vault upload error: {}", env.message);
    }
    let data = env.data.context("missing upload payload")?;
    Ok((data.item, data.quota))
}

pub fn berth_vault_download(
    paths: &BerthPaths,
    vault_id: &str,
    decryption: &BundleDecryption,
) -> Result<(VaultRecordSummary, UnpackedBundle)> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let url_path = format!("/api/v1/berth/vault/sessions/{}", urlencoding::encode(vault_id));
    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "GET",
        &url_path,
        &[],
        None,
        MAX_VAULT_DOWNLOAD_BYTES,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault download failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    let meta_b64 = resp
        .headers
        .get("x-berth-vault-meta")
        .context("missing X-Berth-Vault-Meta on download")?;
    let meta_raw = base64::engine::general_purpose::STANDARD.decode(meta_b64)?;
    let summary: VaultRecordSummary = serde_json::from_slice(&meta_raw)?;
    let unpacked = unpack_bundle(
        &resp.body,
        Some(&summary.sha256),
        decryption,
        Some(&summary.logical_id),
    )?;
    Ok((summary, unpacked))
}

pub fn berth_vault_patch(
    paths: &BerthPaths,
    vault_id: &str,
    title: Option<&str>,
    description: Option<&str>,
    pinned: Option<bool>,
) -> Result<VaultRecordSummary> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "title": title,
        "description": description,
        "pinned": pinned,
    }))?;
    let url_path = format!("/api/v1/berth/vault/sessions/{}", urlencoding::encode(vault_id));
    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "PATCH",
        &url_path,
        &[("Content-Type", "application/json".to_string())],
        Some(&payload),
        1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault patch failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    let env: BerthApiEnvelope<VaultRecordSummary> = serde_json::from_slice(&resp.body)?;
    env.data.context("missing patched session data")
}

pub fn berth_vault_delete(paths: &BerthPaths, vault_id: &str) -> Result<BerthQuotaInfo> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let url_path = format!("/api/v1/berth/vault/sessions/{}", urlencoding::encode(vault_id));
    let resp = vault_http_request(
        &cfg.central_url,
        &token,
        "DELETE",
        &url_path,
        &[],
        None,
        1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault delete failed (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    #[derive(Deserialize)]
    struct DeletePayload {
        quota: BerthQuotaInfo,
    }
    let env: BerthApiEnvelope<DeletePayload> = serde_json::from_slice(&resp.body)?;
    Ok(env.data.context("missing delete quota payload")?.quota)
}

// ---------------------------------------------------------------------------
// Backend 2: Local Directory Vault (`local`) — Atomic No-Clobber Objects + Tombstones
// ---------------------------------------------------------------------------

pub fn local_dir_load_index(dir: &Path) -> Result<StoreIndexFile> {
    let idx_path = dir.join(LOCAL_INDEX_FILE);
    if !idx_path.is_file() {
        return Ok(StoreIndexFile {
            schema_version: 1,
            updated_at: Utc::now(),
            last_change_seq: 0,
            records: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        });
    }
    let raw = String::from_utf8(read_regular_file_no_symlink(&idx_path)?)?;
    let idx: StoreIndexFile = serde_json::from_str(&raw)?;
    validate_store_index(&idx)?;
    Ok(idx)
}

pub fn local_dir_save_index(dir: &Path, idx: &StoreIndexFile) -> Result<()> {
    validate_store_index(idx)?;
    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(idx)?;
    write_private_file_atomic(&dir.join(LOCAL_INDEX_FILE), &bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

pub fn local_dir_put_bundle(
    dir: &Path,
    bundle: &BuiltBundle,
    pinned: bool,
) -> Result<VaultRecordSummary> {
    local_dir_put_bundle_with_redacted(dir, bundle, pinned, true)
}

pub fn local_dir_put_bundle_with_redacted(
    dir: &Path,
    bundle: &BuiltBundle,
    pinned: bool,
    redacted: bool,
) -> Result<VaultRecordSummary> {
    if !is_valid_sha256_hex(&bundle.sha256) {
        bail!("invalid bundle sha256 `{}`", bundle.sha256);
    }
    let objects_dir = dir.join("objects").join(&bundle.sha256[..2]);
    fs::create_dir_all(&objects_dir)?;
    let dest = objects_dir.join(format!("{}.bhv1", bundle.sha256));

    // No-clobber atomic publication via temp + hard_link (works on Unix & Windows NTFS).
    if dest.exists() {
        let existing = read_regular_file_no_symlink(&dest)?;
        if sha256_hex(&existing) != bundle.sha256 {
            bail!("digest integrity conflict on existing object {}", dest.display());
        }
    } else {
        let tmp = objects_dir.join(format!(".tmp-{}", uuid::Uuid::now_v7()));
        write_private_file_atomic(&tmp, &bundle.bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Err(err) = fs::hard_link(&tmp, &dest) {
            if !dest.exists() {
                let _ = fs::remove_file(&tmp);
                return Err(err.into());
            }
        }
        let _ = fs::remove_file(&tmp);
    }

    let mut idx = local_dir_load_index(dir)?;
    let next_rev = idx
        .records
        .values()
        .filter(|r| r.logical_id == bundle.manifest.logical_id)
        .map(|r| r.revision)
        .max()
        .unwrap_or(0)
        + 1;
    let vault_id = format!("loc_{}", &bundle.sha256[..16]);
    let summary = summary_from_bundle(&vault_id, next_rev, bundle, pinned, redacted);
    idx.tombstones.remove(&bundle.sha256);
    idx.records.insert(vault_id.clone(), summary.clone());
    idx.updated_at = Utc::now();
    local_dir_save_index(dir, &idx)?;
    Ok(summary)
}

pub fn local_dir_get_bundle(
    dir: &Path,
    vault_id: &str,
    decryption: &BundleDecryption,
) -> Result<(VaultRecordSummary, UnpackedBundle)> {
    let idx = local_dir_load_index(dir)?;
    let summary = idx
        .records
        .get(vault_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("session `{vault_id}` not found in local store"))?;
    let blob_path = dir
        .join("objects")
        .join(&summary.sha256[..2])
        .join(format!("{}.bhv1", summary.sha256));
    let raw = fs::read(&blob_path)?;
    let unpacked = unpack_bundle(
        &raw,
        Some(&summary.sha256),
        decryption,
        Some(&summary.logical_id),
    )?;
    Ok((summary, unpacked))
}

pub fn local_dir_delete(dir: &Path, vault_id: &str) -> Result<()> {
    let mut idx = local_dir_load_index(dir)?;
    let Some(rec) = idx.records.remove(vault_id) else {
        bail!("session `{vault_id}` not found in local store");
    };
    idx.tombstones.insert(
        rec.sha256.clone(),
        TombstoneEntry {
            vault_id: rec.vault_id.clone(),
            logical_id: rec.logical_id.clone(),
            sha256: rec.sha256.clone(),
            deleted_at: Utc::now(),
        },
    );
    idx.updated_at = Utc::now();
    local_dir_save_index(dir, &idx)?;
    let still_referenced = idx.records.values().any(|r| r.sha256 == rec.sha256);
    if !still_referenced {
        let blob_path = dir
            .join("objects")
            .join(&rec.sha256[..2])
            .join(format!("{}.bhv1", rec.sha256));
        let _ = fs::remove_file(blob_path);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Backend 3: Git / GitHub Remote Sync (`git`) — Hardened Persistent Mirror
// ---------------------------------------------------------------------------

fn hardened_git_cmd(work_dir: &Path, empty_hooks_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(work_dir)
        .arg("-c")
        .arg(format!("core.hooksPath={}", empty_hooks_dir.display()))
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("protocol.allow=never")
        .arg("-c")
        .arg("protocol.https.allow=always")
        .arg("-c")
        .arg("protocol.ssh.allow=always")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=15",
        );
    cmd
}

pub fn git_mirror_dir(paths: &BerthPaths, store_name: &str) -> Result<PathBuf> {
    validate_store_name(store_name)?;
    Ok(paths.state_dir.join("git-mirrors").join(store_name))
}

/// Synchronizes the local mirror of a Git/GitHub vault store:
/// 1. Verifies origin URL matches configured store URL (preventing push to stale remote)
/// 2. Pulls remote branch & validates every remote digest (tombstones applied first!)
/// 3. Merges store-partitioned local cache & mirror indexes respecting `SyncDirection`
/// 4. Commits & fast-forward pushes (never force-pushes!)
pub fn sync_git_store(
    paths: &BerthPaths,
    store_name: &str,
    url: &str,
    branch: &str,
    direction: SyncDirection,
    dry_run: bool,
) -> Result<SyncReport> {
    validate_store_name(store_name)?;
    validate_git_remote_args(url, branch)?;
    let mirror = git_mirror_dir(paths, store_name)?;
    let hooks = paths.state_dir.join("empty-hooks");
    fs::create_dir_all(&mirror)?;
    fs::create_dir_all(&hooks)?;

    let local_cache = store_cache_root(paths, store_name)?;
    fs::create_dir_all(&local_cache)?;

    if !mirror.join(".git").exists() {
        run_checked(
            hardened_git_cmd(&mirror, &hooks).args(["init", "-q"]),
            "git init",
        )?;
        run_checked(
            hardened_git_cmd(&mirror, &hooks).args(["remote", "add", "origin", url]),
            "git remote add",
        )?;
    } else {
        // Verify configured origin matches store URL (update if store was replaced).
        let out = hardened_git_cmd(&mirror, &hooks)
            .args(["remote", "get-url", "origin"])
            .output()
            .context("check git origin URL")?;
        let current_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if current_url != url {
            run_checked(
                hardened_git_cmd(&mirror, &hooks).args(["remote", "set-url", "origin", url]),
                "git remote set-url",
            )?;
        }
    }

    // Fetch remote branch if it exists on origin.
    let fetch_status = hardened_git_cmd(&mirror, &hooks)
        .args(["fetch", "--depth=1", "origin", branch])
        .status()
        .context("run git fetch")?;
    let remote_exists = fetch_status.success();
    if remote_exists {
        run_checked(
            hardened_git_cmd(&mirror, &hooks).args(["checkout", "-B", branch, "FETCH_HEAD"]),
            "git checkout remote branch",
        )?;
    } else {
        run_checked(
            hardened_git_cmd(&mirror, &hooks).args(["checkout", "-B", branch]),
            "git checkout new branch",
        )?;
    }

    let mut local_idx = local_dir_load_index(&local_cache)?;
    let mut remote_idx = local_dir_load_index(&mirror)?;
    let (pulled, pushed, tombstones_applied) = merge_two_dir_stores_directional(
        &local_cache,
        &mut local_idx,
        &mirror,
        &mut remote_idx,
        direction,
        dry_run,
    )?;

    let should_push = direction != SyncDirection::PullOnly;
    if !dry_run && should_push && (pushed > 0 || tombstones_applied > 0 || !remote_exists) {
        // Ensure binary .gitattributes so git never mangles .bhv1 files.
        let gitattrs = mirror.join(".gitattributes");
        if !gitattrs.exists() {
            fs::write(&gitattrs, "*.bhv1 binary -diff -merge\n")?;
        }
        run_checked(
            hardened_git_cmd(&mirror, &hooks).args(["add", "-A"]),
            "git add",
        )?;
        let diff_clean = hardened_git_cmd(&mirror, &hooks)
            .args(["diff", "--cached", "--quiet"])
            .status()?
            .success();
        if !diff_clean {
            run_checked(
                hardened_git_cmd(&mirror, &hooks)
                    .args([
                        "-c",
                        "user.name=berth-vault",
                        "-c",
                        "user.email=vault@berth.local",
                        "commit",
                        "-m",
                        &format!("berth vault sync (pushed={pushed}, tombstones={tombstones_applied})"),
                    ]),
                "git commit",
            )?;
            run_checked(
                hardened_git_cmd(&mirror, &hooks).args(["push", "origin", branch]),
                "git push (fast-forward only)",
            )?;
        }
    }

    Ok(SyncReport {
        store_name: store_name.to_string(),
        pushed,
        pulled,
        tombstones_applied,
        dry_run,
    })
}

// ---------------------------------------------------------------------------
// Backend 4: OpenSSH SFTP / SCP (`sftp://` & `scp://`) — Zero rclone/sshpass/remote-shell
// ---------------------------------------------------------------------------

fn resolve_sftp_binary() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let sys_sftp = PathBuf::from(r"C:\Windows\System32\OpenSSH\sftp.exe");
        if sys_sftp.is_file() {
            return Ok(sys_sftp);
        }
    }
    which::which("sftp").context("OpenSSH `sftp` binary not found in PATH")
}

fn build_sftp_command(
    sftp_bin: &Path,
    target: &str,
    port: Option<u16>,
    identity_file: Option<&Path>,
    known_hosts_file: &Path,
) -> Command {
    let mut cmd = Command::new(sftp_bin);
    cmd.arg("-b")
        .arg("-")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("UpdateHostKeys=no")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts_file.display()));
    if let Some(p) = port {
        cmd.arg("-P").arg(p.to_string());
    }
    if let Some(id) = identity_file {
        cmd.arg("-i").arg(id);
    }
    cmd.arg(target);
    cmd
}

fn run_sftp_batch(
    sftp_bin: &Path,
    target: &str,
    port: Option<u16>,
    identity_file: Option<&Path>,
    known_hosts_file: &Path,
    batch_commands: &str,
) -> Result<String> {
    let mut child = build_sftp_command(sftp_bin, target, port, identity_file, known_hosts_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn OpenSSH sftp")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(batch_commands.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "sftp failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn sync_sftp_store(
    paths: &BerthPaths,
    store_name: &str,
    target: &str,
    port: Option<u16>,
    remote_dir: &str,
    identity_file: Option<&Path>,
    known_hosts_file: Option<&Path>,
    direction: SyncDirection,
    dry_run: bool,
) -> Result<SyncReport> {
    validate_store_name(store_name)?;
    // Re-validate remote_dir against SFTP batch injection characters.
    if remote_dir
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b < 0x20 || matches!(b, b'"' | b'\'' | b'\\'))
        || remote_dir.split('/').any(|seg| seg == "..")
    {
        bail!("invalid SFTP remote_dir `{remote_dir}`");
    }
    let sftp_bin = resolve_sftp_binary()?;
    let kh = known_hosts_file
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.state_dir.join("known_hosts"));
    if !kh.is_file() {
        bail!(
            "pinned SSH known_hosts file `{}` not found; run `ssh-keyscan -H <host> >> {}` first to pin the host key",
            kh.display(),
            kh.display()
        );
    }

    let local_cache = store_cache_root(paths, store_name)?;
    fs::create_dir_all(&local_cache)?;
    let tmp = tempfile::Builder::new().prefix("berth-sftp-sync-").tempdir()?;
    let remote_idx_tmp = tmp.path().join(LOCAL_INDEX_FILE);
    let rdir = remote_dir.trim_end_matches('/');

    // Ensure remote root & objects directory exist (ignore mkdir errors with `-mkdir`).
    let init_batch = format!(
        "-mkdir \"{rdir}\"\n-mkdir \"{rdir}/objects\"\n-get \"{rdir}/{LOCAL_INDEX_FILE}\" \"{}\"\n",
        remote_idx_tmp.display()
    );
    let _ = run_sftp_batch(&sftp_bin, target, port, identity_file, &kh, &init_batch)?;

    let mut remote_idx: StoreIndexFile = if remote_idx_tmp.is_file() {
        let parsed: StoreIndexFile = serde_json::from_str(&fs::read_to_string(&remote_idx_tmp)?)?;
        // P0 SECURITY GATE: validate EVERY remote sha256/vault_id strictly before ANY path or SFTP batch line!
        validate_store_index(&parsed)?;
        parsed
    } else {
        StoreIndexFile {
            schema_version: 1,
            updated_at: Utc::now(),
            last_change_seq: 0,
            records: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        }
    };
    let mut local_idx = local_dir_load_index(&local_cache)?;

    let mut tombstones_applied = 0usize;
    let mut to_pull = Vec::new();
    let mut to_push = Vec::new();

    // 1. Union tombstones respecting SyncDirection (tombstones always win over matching digest!).
    if direction != SyncDirection::PushOnly {
        for (sha, ts) in remote_idx.tombstones.clone() {
            if !local_idx.tombstones.contains_key(&sha) {
                local_idx.tombstones.insert(sha.clone(), ts);
                tombstones_applied += 1;
            }
        }
    }
    if direction != SyncDirection::PullOnly {
        for (sha, ts) in local_idx.tombstones.clone() {
            if !remote_idx.tombstones.contains_key(&sha) {
                remote_idx.tombstones.insert(sha.clone(), ts);
                tombstones_applied += 1;
            }
        }
    }
    local_idx
        .records
        .retain(|_, r| !local_idx.tombstones.contains_key(&r.sha256));
    remote_idx
        .records
        .retain(|_, r| !remote_idx.tombstones.contains_key(&r.sha256));

    if direction != SyncDirection::PushOnly {
        for (vid, rec) in &remote_idx.records {
            if !local_idx.records.contains_key(vid) {
                to_pull.push(rec.clone());
            }
        }
    }
    if direction != SyncDirection::PullOnly {
        for (vid, rec) in &local_idx.records {
            if !remote_idx.records.contains_key(vid) {
                to_push.push(rec.clone());
            }
        }
    }

    if !dry_run {
        // 2. Pull missing objects from SFTP, verifying SHA-256 on every download.
        for rec in &to_pull {
            if !is_valid_sha256_hex(&rec.sha256) {
                bail!("refusing invalid remote sha256 `{}`", rec.sha256);
            }
            let local_obj_dir = local_cache.join("objects").join(&rec.sha256[..2]);
            fs::create_dir_all(&local_obj_dir)?;
            let local_dest = local_obj_dir.join(format!("{}.bhv1", rec.sha256));
            if local_dest.exists() {
                let existing = read_regular_file_no_symlink(&local_dest)?;
                if sha256_hex(&existing) != rec.sha256 {
                    bail!("local cache object SHA-256 mismatch for {}", rec.vault_id);
                }
            } else {
                let stage = tmp.path().join(format!("pull-{}.bhv1", rec.sha256));
                let batch = format!(
                    "get \"{rdir}/objects/{}/{}.bhv1\" \"{}\"\n",
                    &rec.sha256[..2],
                    rec.sha256,
                    stage.display()
                );
                run_sftp_batch(&sftp_bin, target, port, identity_file, &kh, &batch)?;
                let bytes = fs::read(&stage)?;
                if sha256_hex(&bytes) != rec.sha256 {
                    bail!("SFTP downloaded object SHA-256 mismatch for {}", rec.vault_id);
                }
                write_private_file_atomic(&local_dest, &bytes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            local_idx.records.insert(rec.vault_id.clone(), rec.clone());
        }

        // 3. Push missing objects via verified upload + strict rename (verifying remote target exists!).
        let mut pushed_actual = 0usize;
        for rec in &to_push {
            if !is_valid_sha256_hex(&rec.sha256) {
                bail!("refusing invalid local sha256 `{}`", rec.sha256);
            }
            let local_src = local_cache
                .join("objects")
                .join(&rec.sha256[..2])
                .join(format!("{}.bhv1", rec.sha256));
            let local_bytes = read_regular_file_no_symlink(&local_src).with_context(|| {
                format!(
                    "missing local object {} required for SFTP push of {}",
                    local_src.display(),
                    rec.vault_id
                )
            })?;
            if sha256_hex(&local_bytes) != rec.sha256 {
                bail!("local object SHA-256 corrupted for {}", rec.vault_id);
            }
            let prefix = &rec.sha256[..2];
            let remote_tmp = format!("{rdir}/objects/{prefix}/.tmp-{}.bhv1", uuid::Uuid::now_v7());
            let remote_final = format!("{rdir}/objects/{prefix}/{}.bhv1", rec.sha256);
            // Upload temp, attempt rename (fails if destination already exists on SFTPv3),
            // then assert final object exists via `ls` so we never acknowledge an unrestorable backup!
            let batch = format!(
                "-mkdir \"{rdir}/objects/{prefix}\"\nput \"{}\" \"{remote_tmp}\"\n-rename \"{remote_tmp}\" \"{remote_final}\"\n-rm \"{remote_tmp}\"\nls \"{remote_final}\"\n",
                local_src.display()
            );
            run_sftp_batch(&sftp_bin, target, port, identity_file, &kh, &batch)?;
            remote_idx.records.insert(rec.vault_id.clone(), rec.clone());
            pushed_actual += 1;
        }

        // 4. Persist updated manifests on both ends (publish remote index with posix-rename fallback).
        if direction != SyncDirection::PushOnly || tombstones_applied > 0 {
            local_idx.updated_at = Utc::now();
            local_dir_save_index(&local_cache, &local_idx)?;
        }

        if direction != SyncDirection::PullOnly && (pushed_actual > 0 || tombstones_applied > 0) {
            remote_idx.updated_at = Utc::now();
            let push_idx_tmp = tmp.path().join("push-index.json");
            fs::write(&push_idx_tmp, serde_json::to_vec_pretty(&remote_idx)?)?;
            let remote_idx_stage = format!("{rdir}/.tmp-index-{}.json", uuid::Uuid::now_v7());
            // First try POSIX atomic `rename` (OpenSSH posix-rename@openssh.com); fall back to backup-swap if SFTPv3 refuses overwrite.
            let atomic_batch = format!(
                "put \"{}\" \"{remote_idx_stage}\"\nrename \"{remote_idx_stage}\" \"{rdir}/{LOCAL_INDEX_FILE}\"\n",
                push_idx_tmp.display()
            );
            if run_sftp_batch(&sftp_bin, target, port, identity_file, &kh, &atomic_batch).is_err() {
                let backup = format!("{rdir}/.{LOCAL_INDEX_FILE}.prev");
                let swap_batch = format!(
                    "-rm \"{backup}\"\n-rename \"{rdir}/{LOCAL_INDEX_FILE}\" \"{backup}\"\nrename \"{remote_idx_stage}\" \"{rdir}/{LOCAL_INDEX_FILE}\"\n-rm \"{backup}\"\n"
                );
                run_sftp_batch(&sftp_bin, target, port, identity_file, &kh, &swap_batch)?;
            }
        }
    }

    Ok(SyncReport {
        store_name: store_name.to_string(),
        pushed: to_push.len(),
        pulled: to_pull.len(),
        tombstones_applied,
        dry_run,
    })
}

// ---------------------------------------------------------------------------
// Unified Multi-Backend Sync Dispatcher
// ---------------------------------------------------------------------------

pub fn sync_store(
    paths: &BerthPaths,
    store_name_override: Option<&str>,
    direction: SyncDirection,
    dry_run: bool,
) -> Result<SyncReport> {
    let reg = load_store_registry_checked(paths)?;
    let target_name = store_name_override.unwrap_or(&reg.active_store);
    let store = reg
        .stores
        .get(target_name)
        .ok_or_else(|| anyhow::anyhow!("unknown store `{target_name}` (run `bh store` to list stores)"))?;

    match &store.backend {
        StoreBackendConfig::Berth => sync_berth_cloud(paths, target_name, direction, dry_run),
        StoreBackendConfig::Local { path } => {
            let local_cache = store_cache_root(paths, target_name)?;
            fs::create_dir_all(&local_cache)?;
            fs::create_dir_all(path)?;
            let mut a = local_dir_load_index(&local_cache)?;
            let mut b = local_dir_load_index(path)?;
            let (pulled, pushed, tombstones_applied) = merge_two_dir_stores_directional(
                &local_cache,
                &mut a,
                path,
                &mut b,
                direction,
                dry_run,
            )?;
            Ok(SyncReport {
                store_name: target_name.to_string(),
                pushed,
                pulled,
                tombstones_applied,
                dry_run,
            })
        }
        StoreBackendConfig::Git { url, branch } => {
            sync_git_store(paths, target_name, url, branch, direction, dry_run)
        }
        StoreBackendConfig::Sftp {
            target,
            port,
            remote_dir,
            identity_file,
            known_hosts_file,
        } => sync_sftp_store(
            paths,
            target_name,
            target,
            *port,
            remote_dir,
            identity_file.as_deref(),
            known_hosts_file.as_deref(),
            direction,
            dry_run,
        ),
    }
}

fn berth_vault_list_changes(
    paths: &BerthPaths,
    since_seq: i64,
    limit: usize,
) -> Result<(Vec<VaultRecordSummary>, i64)> {
    let mut cfg = load_berth_config(paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (token, _) = resolve_vault_owner_token(&mut cfg, paths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = format!("/api/v1/berth/vault/changes?since={since_seq}&limit={limit}");
    let resp = vault_http_request(&cfg.central_url, &token, "GET", &path, &[], None, 8 * 1024 * 1024)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status != 200 {
        bail!(
            "Berth Cloud Vault changes error (HTTP {}): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    #[derive(Deserialize)]
    struct ChangesPayload {
        items: Vec<VaultRecordSummary>,
        next_seq: i64,
    }
    let env: BerthApiEnvelope<ChangesPayload> = serde_json::from_slice(&resp.body)?;
    if env.code != 0 {
        bail!("Berth Cloud Vault changes error: {}", env.message);
    }
    let data = env.data.context("missing changes payload")?;
    Ok((data.items, data.next_seq))
}

fn sync_berth_cloud(
    paths: &BerthPaths,
    store_name: &str,
    direction: SyncDirection,
    dry_run: bool,
) -> Result<SyncReport> {
    let local_cache = store_cache_root(paths, store_name)?;
    fs::create_dir_all(&local_cache)?;
    let mut local_idx = local_dir_load_index(&local_cache)?;

    let mut pulled = 0usize;
    let mut pushed = 0usize;
    let mut tombstones_applied = 0usize;

    // 1. Paginate `/api/v1/berth/vault/changes?since=<cursor>` to full convergence (live + tombstones).
    let mut cursor = local_idx.last_change_seq;
    let mut remote_live_shas = std::collections::HashSet::new();
    let mut remote_tombstone_shas = std::collections::HashSet::new();

    loop {
        let (page, next_seq) = berth_vault_list_changes(paths, cursor, 200)?;
        if page.is_empty() {
            break;
        }
        for item in &page {
            if !is_valid_sha256_hex(&item.sha256) {
                bail!("invalid sha256 `{}` from Berth Cloud Vault", item.sha256);
            }
            if let Some(del_at) = item.deleted_at {
                let tsha = item
                    .tombstone_sha256
                    .clone()
                    .unwrap_or_else(|| item.sha256.clone());
                remote_tombstone_shas.insert(tsha.clone());
                if direction != SyncDirection::PushOnly {
                    let had_local = local_idx.records.values().any(|r| r.sha256 == tsha)
                        || !local_idx.tombstones.contains_key(&tsha);
                    if had_local {
                        tombstones_applied += 1;
                    }
                    if !dry_run {
                        local_idx.tombstones.insert(
                            tsha.clone(),
                            TombstoneEntry {
                                vault_id: item.vault_id.clone(),
                                logical_id: item.logical_id.clone(),
                                sha256: tsha.clone(),
                                deleted_at: del_at,
                            },
                        );
                        local_idx.records.retain(|_, r| r.sha256 != tsha);
                        let blob = local_cache
                            .join("objects")
                            .join(&tsha[..2])
                            .join(format!("{tsha}.bhv1"));
                        let _ = fs::remove_file(&blob);
                    }
                }
            } else {
                remote_live_shas.insert(item.sha256.clone());
                if direction != SyncDirection::PushOnly {
                    let exists_local = local_idx.records.values().any(|r| r.sha256 == item.sha256);
                    if !exists_local && !local_idx.tombstones.contains_key(&item.sha256) {
                        pulled += 1;
                        if !dry_run {
                            local_idx.records.insert(item.vault_id.clone(), item.clone());
                        }
                    }
                }
            }
        }
        if next_seq <= cursor {
            break;
        }
        cursor = next_seq;
        if page.len() < 200 {
            break;
        }
    }
    if !dry_run && direction != SyncDirection::PushOnly {
        local_idx.last_change_seq = cursor;
    }

    // 2. Propagate local tombstones to Berth Cloud Vault before uploading new items.
    if direction != SyncDirection::PullOnly {
        for (tsha, ts) in local_idx.tombstones.clone() {
            if remote_live_shas.contains(&tsha) && !remote_tombstone_shas.contains(&tsha) {
                tombstones_applied += 1;
                if !dry_run && ts.vault_id.starts_with("vlt_") {
                    let _ = berth_vault_delete(paths, &ts.vault_id);
                }
            }
        }

        // 3. Push local cached bundles belonging to THIS Berth store partition that are missing in cloud.
        let mut live_remote_all: std::collections::HashSet<String> = remote_live_shas;
        // Include any previously pulled records already holding a server `vlt_` ID so we don't re-upload.
        for rec in local_idx.records.values() {
            if rec.vault_id.starts_with("vlt_") {
                live_remote_all.insert(rec.sha256.clone());
            }
        }
        for rec in local_idx.records.values().cloned().collect::<Vec<_>>() {
            if live_remote_all.contains(&rec.sha256)
                || remote_tombstone_shas.contains(&rec.sha256)
                || local_idx.tombstones.contains_key(&rec.sha256)
            {
                continue;
            }
            let blob_path = local_cache
                .join("objects")
                .join(&rec.sha256[..2])
                .join(format!("{}.bhv1", rec.sha256));
            if !blob_path.is_file() {
                continue;
            }
            pushed += 1;
            if !dry_run {
                let bytes = read_regular_file_no_symlink(&blob_path)?;
                if sha256_hex(&bytes) != rec.sha256 {
                    bail!("corrupted local cached bundle {}", blob_path.display());
                }
                let sealed_meta = rec
                    .sealed_meta_b64
                    .as_deref()
                    .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok());
                let built = BuiltBundle {
                    manifest: BundleManifest {
                        schema_version: 1,
                        bundle_id: rec.vault_id.clone(),
                        logical_id: rec.logical_id.clone(),
                        created_at: rec.updated_at,
                        producer_version: env!("CARGO_PKG_VERSION").into(),
                        producer_os: std::env::consts::OS.into(),
                        producer_arch: std::env::consts::ARCH.into(),
                        provider: rec.provider.clone(),
                        source_session_id: rec.source_session_id.clone(),
                        origin: "sync".into(),
                        title: rec.title.clone(),
                        description: rec.description.clone(),
                        workspace_hint: rec.workspace_hint.clone(),
                        git_repo: rec.git_repo.clone(),
                        git_branch: rec.git_branch.clone(),
                        tags: rec.tags.clone(),
                        message_count: rec.message_count.max(0) as usize,
                        files: vec![],
                        redaction: crate::redact::RedactionReport::new(),
                    },
                    bytes,
                    raw_bytes: rec.raw_bytes.max(0) as u64,
                    stored_bytes: rec.stored_bytes.max(0) as u64,
                    sha256: rec.sha256.clone(),
                    encoding: rec.encoding.clone(),
                    encrypted: rec.encrypted,
                    sealed_meta,
                };
                let (uploaded, _) = berth_vault_upload(paths, &built, rec.pinned, rec.redacted)?;
                local_idx.records.remove(&rec.vault_id);
                local_idx.records.insert(uploaded.vault_id.clone(), uploaded);
            }
        }
    }

    if !dry_run {
        local_idx.updated_at = Utc::now();
        local_dir_save_index(&local_cache, &local_idx)?;
    }

    Ok(SyncReport {
        store_name: store_name.to_string(),
        pushed,
        pulled,
        tombstones_applied,
        dry_run,
    })
}

#[cfg(test)]
fn merge_two_dir_stores(
    local_dir: &Path,
    local_idx: &mut StoreIndexFile,
    remote_dir: &Path,
    remote_idx: &mut StoreIndexFile,
    dry_run: bool,
) -> Result<(usize, usize, usize)> {
    merge_two_dir_stores_directional(
        local_dir,
        local_idx,
        remote_dir,
        remote_idx,
        SyncDirection::Both,
        dry_run,
    )
}

fn merge_two_dir_stores_directional(
    local_dir: &Path,
    local_idx: &mut StoreIndexFile,
    remote_dir: &Path,
    remote_idx: &mut StoreIndexFile,
    direction: SyncDirection,
    dry_run: bool,
) -> Result<(usize, usize, usize)> {
    validate_store_index(local_idx)?;
    validate_store_index(remote_idx)?;
    let mut tombstones_applied = 0usize;
    if direction != SyncDirection::PushOnly {
        for (sha, ts) in remote_idx.tombstones.clone() {
            if !local_idx.tombstones.contains_key(&sha) {
                local_idx.tombstones.insert(sha, ts);
                tombstones_applied += 1;
            }
        }
    }
    if direction != SyncDirection::PullOnly {
        for (sha, ts) in local_idx.tombstones.clone() {
            if !remote_idx.tombstones.contains_key(&sha) {
                remote_idx.tombstones.insert(sha, ts);
                tombstones_applied += 1;
            }
        }
    }
    local_idx
        .records
        .retain(|_, r| !local_idx.tombstones.contains_key(&r.sha256));
    remote_idx
        .records
        .retain(|_, r| !remote_idx.tombstones.contains_key(&r.sha256));

    let mut pulled = 0usize;
    let mut pushed = 0usize;

    if direction != SyncDirection::PushOnly {
        for (vid, rec) in remote_idx.records.clone() {
            if !local_idx.records.contains_key(&vid) {
                pulled += 1;
                if !dry_run {
                    copy_verified_object(remote_dir, local_dir, &rec.sha256)?;
                    local_idx.records.insert(vid, rec);
                }
            }
        }
    }
    if direction != SyncDirection::PullOnly {
        for (vid, rec) in local_idx.records.clone() {
            if !remote_idx.records.contains_key(&vid) {
                pushed += 1;
                if !dry_run {
                    copy_verified_object(local_dir, remote_dir, &rec.sha256)?;
                    remote_idx.records.insert(vid, rec);
                }
            }
        }
    }

    if !dry_run {
        if direction != SyncDirection::PushOnly || tombstones_applied > 0 {
            local_idx.updated_at = Utc::now();
            local_dir_save_index(local_dir, local_idx)?;
        }
        if direction != SyncDirection::PullOnly || tombstones_applied > 0 {
            remote_idx.updated_at = Utc::now();
            local_dir_save_index(remote_dir, remote_idx)?;
        }
    }

    Ok((pulled, pushed, tombstones_applied))
}

fn copy_verified_object(src_root: &Path, dst_root: &Path, sha: &str) -> Result<()> {
    if !is_valid_sha256_hex(sha) {
        bail!("invalid object sha256 `{sha}`");
    }
    let rel = Path::new("objects").join(&sha[..2]).join(format!("{sha}.bhv1"));
    let src = src_root.join(&rel);
    let dst = dst_root.join(&rel);
    if dst.exists() {
        let existing = read_regular_file_no_symlink(&dst)?;
        if sha256_hex(&existing) != sha {
            bail!(
                "corrupted destination object {} (SHA-256 mismatch)",
                dst.display()
            );
        }
        return Ok(());
    }
    let bytes = read_regular_file_no_symlink(&src).with_context(|| {
        format!(
            "missing required source object {} for digest {sha}",
            src.display()
        )
    })?;
    if sha256_hex(&bytes) != sha {
        bail!("corrupted object {} (SHA-256 mismatch)", src.display());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private_file_atomic(&dst, &bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// High-level helper: resolves target store and its `encrypt_by_default` policy FIRST,
/// packages a native session file with default secret redaction, zstd compression, and
/// optional `age` E2EE (with opaque E2EE `logical_id`), caches in THAT store's partition,
/// and uploads/syncs to the target store.
#[allow(clippy::too_many_arguments)]
pub fn save_session_file_to_store(
    paths: &BerthPaths,
    berth_cfg: Option<&BerthConfig>,
    provider: &str,
    session_id: &str,
    origin: &str,
    source_file: &Path,
    title: &str,
    description: &str,
    workspace_hint: &str,
    git_branch: &str,
    message_count: usize,
    encrypt: bool,
    passphrase: Option<&str>,
    no_redact: bool,
    store_override: Option<&str>,
    pinned: bool,
) -> Result<(VaultRecordSummary, Option<BerthQuotaInfo>, BuiltBundle)> {
    // 1. Resolve destination store & effective encryption policy FIRST (fixes CLI ignoring store default!).
    let reg = load_store_registry_checked(paths)?;
    let active_name = store_override.unwrap_or(&reg.active_store).to_string();
    let store = reg
        .stores
        .get(&active_name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown store `{active_name}`"))?;
    let effective_encrypt = encrypt || store.encrypt_by_default;

    // 2. Read source file safely (refusing symlinks via O_NOFOLLOW).
    let bytes = read_regular_file_no_symlink(source_file)
        .with_context(|| format!("read session file {}", source_file.display()))?;
    let rel_name = source_file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session.jsonl");
    let rel_path = match provider {
        "codex" => format!("sessions/imported/{rel_name}"),
        "claude-code" => format!("projects/imported/{rel_name}"),
        "grok" => format!("sessions/imported/{session_id}/updates.jsonl"),
        _ => format!("native/{rel_name}"),
    };

    // 3. When E2EE is active, use an opaque outer `logical_id` so the native session ID is never leaked!
    let logical_id = if effective_encrypt {
        let digest = sha256_hex(format!("berth-e2ee-v1:{provider}:{session_id}").as_bytes());
        format!("e2ee_{}", &digest[..24])
    } else {
        format!("{provider}:{session_id}")
    };

    let manifest = BundleManifest {
        schema_version: 1,
        bundle_id: String::new(),
        logical_id,
        created_at: Utc::now(),
        producer_version: env!("CARGO_PKG_VERSION").to_string(),
        producer_os: std::env::consts::OS.to_string(),
        producer_arch: std::env::consts::ARCH.to_string(),
        provider: provider.to_string(),
        source_session_id: session_id.to_string(),
        origin: origin.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        workspace_hint: workspace_hint.to_string(),
        git_repo: String::new(),
        git_branch: git_branch.to_string(),
        tags: vec![],
        message_count,
        files: vec![],
        redaction: crate::redact::RedactionReport::new(),
    };

    let redacted = !no_redact;
    let redactor = if no_redact {
        None
    } else {
        Some(Redactor::from_runtime_environment(berth_cfg))
    };

    let encryption = if effective_encrypt {
        if let Some(pass) = passphrase {
            BundleEncryption::Passphrase(pass.to_string())
        } else {
            let id_path = default_age_identity_path(paths);
            let recipient = ensure_age_identity(&id_path)?;
            BundleEncryption::X25519Recipient(recipient)
        }
    } else {
        BundleEncryption::None
    };

    let built = build_bundle(
        manifest,
        vec![BundleInputFile {
            role: "transcript".to_string(),
            rel_path,
            bytes,
        }],
        redactor.as_ref(),
        &encryption,
        9,
    )?;

    // 4. Stage strictly into THIS store's isolated cache partition (never polluting other stores!).
    let store_cache = store_cache_root(paths, &store.name)?;

    match &store.backend {
        StoreBackendConfig::Berth => {
            let (summary, quota) = berth_vault_upload(paths, &built, pinned, redacted)?;
            // Mirror uploaded bundle into the Berth store cache partition for fast local lookup.
            let _ = local_dir_put_bundle_with_redacted(&store_cache, &built, pinned, redacted);
            Ok((summary, Some(quota), built))
        }
        StoreBackendConfig::Local { path } => {
            let _ = local_dir_put_bundle_with_redacted(&store_cache, &built, pinned, redacted)?;
            let summary = local_dir_put_bundle_with_redacted(path, &built, pinned, redacted)?;
            Ok((summary, None, built))
        }
        StoreBackendConfig::Git { url, branch } => {
            // Stage ONCE into store_cache, then let sync_git_store copy -> commit -> push to origin!
            let summary = local_dir_put_bundle_with_redacted(&store_cache, &built, pinned, redacted)?;
            let _ = sync_git_store(
                paths,
                &store.name,
                url,
                branch,
                SyncDirection::PushOnly,
                false,
            )?;
            Ok((summary, None, built))
        }
        StoreBackendConfig::Sftp {
            target,
            port,
            remote_dir,
            identity_file,
            known_hosts_file,
        } => {
            let summary = local_dir_put_bundle_with_redacted(&store_cache, &built, pinned, redacted)?;
            let _ = sync_sftp_store(
                paths,
                &store.name,
                target,
                *port,
                remote_dir,
                identity_file.as_deref(),
                known_hosts_file.as_deref(),
                SyncDirection::PushOnly,
                false,
            )?;
            Ok((summary, None, built))
        }
    }
}

fn summary_from_bundle(
    vault_id: &str,
    revision: i32,
    bundle: &BuiltBundle,
    pinned: bool,
    redacted: bool,
) -> VaultRecordSummary {
    let sealed_meta_b64 = bundle
        .sealed_meta
        .as_ref()
        .map(|b| base64::engine::general_purpose::STANDARD.encode(b));
    VaultRecordSummary {
        vault_id: vault_id.to_string(),
        logical_id: bundle.manifest.logical_id.clone(),
        revision,
        provider: bundle.manifest.provider.clone(),
        source_session_id: if bundle.encrypted {
            String::new()
        } else {
            bundle.manifest.source_session_id.clone()
        },
        title: if bundle.encrypted {
            "[encrypted]".to_string()
        } else {
            bundle.manifest.title.clone()
        },
        description: if bundle.encrypted {
            String::new()
        } else {
            bundle.manifest.description.clone()
        },
        workspace_hint: if bundle.encrypted {
            String::new()
        } else {
            bundle.manifest.workspace_hint.clone()
        },
        git_repo: if bundle.encrypted {
            String::new()
        } else {
            bundle.manifest.git_repo.clone()
        },
        git_branch: if bundle.encrypted {
            String::new()
        } else {
            bundle.manifest.git_branch.clone()
        },
        tags: if bundle.encrypted {
            vec![]
        } else {
            bundle.manifest.tags.clone()
        },
        message_count: bundle.manifest.message_count as i32,
        raw_bytes: bundle.raw_bytes as i64,
        stored_bytes: bundle.stored_bytes as i64,
        sha256: bundle.sha256.clone(),
        encoding: bundle.encoding.clone(),
        encrypted: bundle.encrypted,
        redacted,
        redacted_count: bundle.manifest.redaction.total_redactions as i32,
        pinned,
        updated_at: Utc::now(),
        deleted_at: None,
        tombstone_sha256: None,
        change_seq: 0,
        sealed_meta_b64,
    }
}

fn run_checked(cmd: &mut Command, label: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("spawn {label}"))?;
    if !out.status.success() {
        bail!(
            "{label} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validates_git_and_sftp_urls_strictly() {
        assert!(validate_git_remote_args("https://github.com/user/vault.git", "main").is_ok());
        assert!(validate_git_remote_args("git@github.com:user/vault.git", "berth-vault").is_ok());
        assert!(validate_git_remote_args("-oProxyCommand=evil", "main").is_err());
        assert!(validate_git_remote_args("ext::sh -c id", "main").is_err());
        assert!(validate_git_remote_args("file:///etc/passwd", "main").is_err());

        let (t, p, d) = parse_sftp_url("sftp://alice@example.com:2222/srv/vault").unwrap();
        assert_eq!(t, "alice@example.com");
        assert_eq!(p, Some(2222));
        assert_eq!(d, "/srv/vault");

        let (t2, p2, d2) = parse_sftp_url("scp://alice@example.com/srv/vault").unwrap();
        assert_eq!(t2, "alice@example.com");
        assert_eq!(p2, None);
        assert_eq!(d2, "/srv/vault");

        // Embedded password forbidden.
        assert!(parse_sftp_url("sftp://alice:secret@example.com/srv/vault").is_err());

        // P0 SFTP batch injection guard: malicious sha256 in remote index.json must fail closed!
        let mut bad_idx = StoreIndexFile::default();
        bad_idx.records.insert(
            "vlt_evil".into(),
            VaultRecordSummary {
                vault_id: "vlt_evil".into(),
                logical_id: "e2ee_1234".into(),
                revision: 1,
                provider: "claude-code".into(),
                source_session_id: String::new(),
                title: String::new(),
                description: String::new(),
                workspace_hint: String::new(),
                git_repo: String::new(),
                git_branch: String::new(),
                tags: vec![],
                message_count: 1,
                raw_bytes: 10,
                stored_bytes: 10,
                sha256: "aaaa\" ; !id #\nget /etc/passwd".into(),
                encoding: "zstd".into(),
                encrypted: false,
                redacted: true,
                redacted_count: 0,
                pinned: false,
                updated_at: Utc::now(),
                deleted_at: None,
                tombstone_sha256: None,
                change_seq: 0,
                sealed_meta_b64: None,
            },
        );
        assert!(validate_store_index(&bad_idx).is_err());
        assert!(validate_store_name("../evil").is_err());
        assert!(validate_store_name("backup_1").is_ok());
    }

    #[test]
    fn local_store_sync_respects_tombstones_and_no_clobber() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let built = build_bundle(
            BundleManifest {
                schema_version: 1,
                bundle_id: String::new(),
                logical_id: "claude-code:s1".into(),
                created_at: Utc::now(),
                producer_version: "2.3.0".into(),
                producer_os: "linux".into(),
                producer_arch: "x86_64".into(),
                provider: "claude-code".into(),
                source_session_id: "s1".into(),
                origin: "host".into(),
                title: "Auth refactor".into(),
                description: String::new(),
                workspace_hint: "/work/app".into(),
                git_repo: String::new(),
                git_branch: "main".into(),
                tags: vec![],
                message_count: 2,
                files: vec![],
                redaction: crate::redact::RedactionReport::new(),
            },
            vec![BundleInputFile {
                role: "transcript".into(),
                rel_path: "projects/work/s1.jsonl".into(),
                bytes: b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n".to_vec(),
            }],
            None,
            &BundleEncryption::None,
            3,
        )
        .unwrap();

        let rec = local_dir_put_bundle(dir_a.path(), &built, false).unwrap();
        let mut idx_a = local_dir_load_index(dir_a.path()).unwrap();
        let mut idx_b = local_dir_load_index(dir_b.path()).unwrap();
        let (pulled, pushed, tombs) =
            merge_two_dir_stores(dir_a.path(), &mut idx_a, dir_b.path(), &mut idx_b, false).unwrap();
        assert_eq!((pulled, pushed, tombs), (0, 1, 0));
        assert!(idx_b.records.contains_key(&rec.vault_id));

        // Delete on B -> tombstone propagates to A on next sync and never resurrects!
        local_dir_delete(dir_b.path(), &rec.vault_id).unwrap();
        let mut idx_a2 = local_dir_load_index(dir_a.path()).unwrap();
        let mut idx_b2 = local_dir_load_index(dir_b.path()).unwrap();
        let (_, _, tombs2) =
            merge_two_dir_stores(dir_a.path(), &mut idx_a2, dir_b.path(), &mut idx_b2, false).unwrap();
        assert_eq!(tombs2, 1);
        assert!(idx_a2.records.is_empty());
        assert!(idx_b2.records.is_empty());
    }
}
