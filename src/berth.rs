//! Berth v2 client configuration, management transport (direct + BHE1),
//! lease lifecycle, ingress probing, isolation enforcement, and per-CLI
//! launch environment generation for unmodified stock CLIs
//! (`codex`, `claude`, `agy`, `grok`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const BERTH_CONFIG_SCHEMA: &str = "ags-berth-v2";
pub const DEFAULT_BERTH_URL: &str = "https://berth.itachi.fans";
pub const AGS_VERSION_HEADER_VALUE: &str = "2";
pub const VALID_ZONES: &[&str] = &["auto", "cn_mainland", "overseas", "home_direct"];
pub const VALID_AGENTS: &[&str] = &["codex", "claude", "agy", "grok"];

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_BHE1_LINE_BYTES: usize = 4096;

const PROXY_ENV_UNSET: &[&str] = &[
    "ALL_PROXY",
    "FTP_PROXY",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "all_proxy",
    "ftp_proxy",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

const CARRIER_ENV_UNSET: &[&str] = &[
    "BERTH_LEASE_TOKEN",
    "AGS_BERTH_LEASE_TOKEN",
    "WSL2_GUI_APPS_ENABLED",
    "WSLENV",
    "WSL_DISTRO",
    "WSL_DISTRO_NAME",
    "WSL_HOST",
    "WSL_INTEROP",
    "WT_PROFILE_ID",
    "WT_SESSION",
];

const ALL_PROVIDER_CREDENTIAL_ENVS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_HOME",
    "CODEXT_POOL_ACCOUNT",
    "CODEXT_POOL_DEVICE_ID",
    "CODEX_ACCESS_TOKEN",
    "CODEX_API_KEY",
    "GEMINI_API_BASE",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GEMINI_BASE_URL",
    "GROK_API_KEY",
    "GROK_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "XAI_API_KEY",
    "XAI_BASE_URL",
];

#[derive(Debug, thiserror::Error)]
pub enum BerthError {
    #[error("invalid berth configuration: {0}")]
    InvalidConfig(String),
    #[error("berth is not configured (run `bh login` or `bh init --url https://berth.itachi.fans --key KEY`)")]
    NotConfigured,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("strict isolation mode unavailable on this host: {0}")]
    StrictIsolationUnavailable(String),
    #[error("berth transport error: {0}")]
    Transport(String),
    #[error("berth API error (HTTP {status}): {body}")]
    ApiStatus { status: u16, body: String },
    #[error("berth I/O error: {0}")]
    Io(String),
    #[error("berth JSON error: {0}")]
    Json(String),
}

impl From<io::Error> for BerthError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for BerthError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

/// Client-side cage isolation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationMode {
    /// Aligns TZ/LANG/LC_ALL, overlay hooks, and scrubs Windows interop PATH.
    /// Does NOT claim OS-level network egress isolation.
    #[default]
    TzOverlay,
    /// Requires Linux + zoneinfo + unprivileged `unshare -Ur -m` + no active
    /// Windows/WSL interop bypass. Fails closed otherwise.
    Strict,
}

impl IsolationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TzOverlay => "tz-overlay",
            Self::Strict => "strict",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, BerthError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tz-overlay" | "tz_overlay" => Ok(Self::TzOverlay),
            "strict" => Ok(Self::Strict),
            other => Err(BerthError::InvalidArgument(format!(
                "isolation mode must be `tz-overlay` or `strict`; got `{other}`"
            ))),
        }
    }
}

impl fmt::Display for IsolationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where AGS persists Berth v2 configuration and runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BerthPaths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
}

impl BerthPaths {
    pub fn new(config_file: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            config_file,
            state_dir,
        }
    }

    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = match std::env::var_os("BERTH_CONFIG_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => match std::env::var_os("XDG_CONFIG_HOME") {
                Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("berth"),
                _ => home.join(".config").join("berth"),
            },
        };
        let state_dir = match std::env::var_os("BERTH_STATE_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => match std::env::var_os("AGENT_SESSION_STATE_DIR") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => match std::env::var_os("XDG_STATE_HOME") {
                    Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("berth"),
                    _ => home.join(".local").join("state").join("berth"),
                },
            },
        };
        Self {
            config_file: config_dir.join("berth.json"),
            state_dir,
        }
    }
}

fn default_schema() -> String {
    BERTH_CONFIG_SCHEMA.to_string()
}

fn default_zone_preference() -> String {
    "auto".to_string()
}

fn default_ags_version() -> String {
    AGS_VERSION_HEADER_VALUE.to_string()
}

fn default_gate_epoch() -> u64 {
    1
}

fn default_weight() -> u32 {
    100
}

/// Authenticated user session saved by `bh login` inside `~/.config/berth/berth.json` (`0600`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAuthSession {
    pub email: String,
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub role: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<i64>,
}

impl fmt::Debug for UserAuthSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserAuthSession")
            .field("email", &self.email)
            .field("user_id", &self.user_id)
            .field("role", &self.role)
            .field("access_token", &redact_secret(&self.access_token))
            .field(
                "refresh_token",
                &self.refresh_token.as_deref().map(redact_secret),
            )
            .field("expires_at_unix", &self.expires_at_unix)
            .finish()
    }
}

impl UserAuthSession {
    pub fn is_expiring_within(&self, margin_secs: i64) -> bool {
        let Some(expires_at) = self.expires_at_unix else {
            return false;
        };
        let now = unix_timestamp_now();
        now.saturating_add(margin_secs) >= expires_at
    }
}

fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Redacts a secret into a short deterministic SHA-256 fingerprint prefix (`sha256:abcdef012345…`).
pub fn redact_secret(secret: &str) -> String {
    if secret.trim().is_empty() {
        return "(none)".to_string();
    }
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    let mut hex_prefix = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex_prefix, "{byte:02x}");
    }
    format!("sha256:{hex_prefix}…")
}

/// Persisted Berth v2 client configuration (`~/.config/berth/berth.json`, mode `0600`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BerthConfig {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub central_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platform_keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_session: Option<UserAuthSession>,
    #[serde(default)]
    pub mgmt_entries: Vec<String>,
    #[serde(default = "default_zone_preference")]
    pub zone_preference: String,
    #[serde(default)]
    pub isolation_mode: IsolationMode,
    #[serde(default)]
    pub allow_insecure_http_for_tests: bool,
}

impl fmt::Debug for BerthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_platform_keys: BTreeMap<&str, String> = self
            .platform_keys
            .iter()
            .map(|(k, v)| (k.as_str(), redact_secret(v)))
            .collect();
        f.debug_struct("BerthConfig")
            .field("schema", &self.schema)
            .field("central_url", &self.central_url)
            .field("api_key", &self.redacted_api_key())
            .field("platform_keys", &redacted_platform_keys)
            .field("user_session", &self.user_session)
            .field("mgmt_entries", &self.mgmt_entries)
            .field("zone_preference", &self.zone_preference)
            .field("isolation_mode", &self.isolation_mode)
            .field(
                "allow_insecure_http_for_tests",
                &self.allow_insecure_http_for_tests,
            )
            .finish()
    }
}

impl BerthConfig {
    pub fn validate_transport(&self) -> Result<(), BerthError> {
        if self.schema != BERTH_CONFIG_SCHEMA {
            return Err(BerthError::InvalidConfig(format!(
                "unsupported schema `{}` (expected `{BERTH_CONFIG_SCHEMA}`)",
                self.schema
            )));
        }
        let parsed = ParsedUrl::parse(&self.central_url)?;
        if !parsed.is_https && !self.allow_insecure_http_for_tests {
            return Err(BerthError::InvalidConfig(
                "central_url must use https:// (set allow_insecure_http_for_tests=true only for local tests)"
                    .into(),
            ));
        }
        if !VALID_ZONES.contains(&self.zone_preference.as_str()) {
            return Err(BerthError::InvalidConfig(format!(
                "zone_preference must be one of {}; got `{}`",
                VALID_ZONES.join("|"),
                self.zone_preference
            )));
        }
        for entry in &self.mgmt_entries {
            validate_host_port(entry)?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), BerthError> {
        self.validate_transport()?;
        if self.api_key.contains('\r') || self.api_key.contains('\n') {
            return Err(BerthError::InvalidConfig(
                "api_key must be a single-line token".into(),
            ));
        }
        for (target, token) in &self.platform_keys {
            if target.trim().is_empty()
                || token.trim().is_empty()
                || token.contains('\r')
                || token.contains('\n')
            {
                return Err(BerthError::InvalidConfig(format!(
                    "platform_keys entry `{target}` must have a non-empty single-line token"
                )));
            }
        }
        if let Some(session) = &self.user_session {
            if session.access_token.trim().is_empty()
                || session.access_token.contains('\r')
                || session.access_token.contains('\n')
            {
                return Err(BerthError::InvalidConfig(
                    "user_session.access_token must be a non-empty single-line token".into(),
                ));
            }
            if let Some(refresh) = &session.refresh_token
                && (refresh.trim().is_empty() || refresh.contains('\r') || refresh.contains('\n'))
            {
                return Err(BerthError::InvalidConfig(
                    "user_session.refresh_token must be a non-empty single-line token".into(),
                ));
            }
        }
        if self.api_key.trim().is_empty()
            && self.platform_keys.is_empty()
            && self.user_session.is_none()
        {
            return Err(BerthError::InvalidConfig(
                "at least one authentication credential is required (run `bh login` or `bh init --url URL --key KEY`)"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn has_any_auth(&self) -> bool {
        !self.api_key.trim().is_empty()
            || !self.platform_keys.is_empty()
            || self.user_session.is_some()
    }

    pub fn redacted_api_key(&self) -> String {
        redact_secret(&self.api_key)
    }

    pub fn redacted_platform_keys(&self) -> BTreeMap<String, String> {
        self.platform_keys
            .iter()
            .map(|(k, v)| (k.clone(), redact_secret(v)))
            .collect()
    }
}

pub fn agent_to_platform(agent: &str) -> Result<&'static str, BerthError> {
    match validate_agent_name(agent)? {
        "codex" => Ok("openai"),
        "claude" => Ok("anthropic"),
        "agy" => Ok("antigravity"),
        "grok" => Ok("grok"),
        _ => unreachable!(),
    }
}

/// Normalizes a `bh key set/unset <TARGET>` argument into either `"default"` or
/// one of the canonical agent names (`codex`, `claude`, `agy`, `grok`).
pub fn canonical_key_target(target: &str) -> Result<&'static str, BerthError> {
    match target.trim().to_ascii_lowercase().as_str() {
        "default" => Ok("default"),
        "codex" | "openai" => Ok("codex"),
        "claude" | "claude-code" | "anthropic" => Ok("claude"),
        "agy" | "antigravity" | "gemini" => Ok("agy"),
        "grok" | "xai" => Ok("grok"),
        other => Err(BerthError::InvalidArgument(format!(
            "key target must be `default`, `codex` (openai), `claude` (anthropic), `agy` (antigravity/gemini), or `grok` (xai); got `{other}`"
        ))),
    }
}

pub fn validate_agent_name(agent: &str) -> Result<&'static str, BerthError> {
    match agent.trim().to_ascii_lowercase().as_str() {
        "codex" => Ok("codex"),
        "claude" | "claude-code" => Ok("claude"),
        "agy" | "antigravity" => Ok("agy"),
        "grok" => Ok("grok"),
        other => Err(BerthError::InvalidArgument(format!(
            "agent must be one of {}; got `{other}`",
            VALID_AGENTS.join("|")
        ))),
    }
}

pub fn validate_timezone_identifier(tz: &str) -> Result<(), BerthError> {
    let trimmed = tz.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains("..")
        || trimmed.contains('\\')
        || !trimmed.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+' | b'.')
        })
    {
        return Err(BerthError::InvalidArgument(format!(
            "invalid IANA timezone `{tz}`"
        )));
    }
    Ok(())
}

fn validate_host_port(raw: &str) -> Result<(), BerthError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err(BerthError::InvalidConfig(format!(
            "invalid mgmt_entry `{raw}`"
        )));
    }
    let Some((host, port_str)) = trimmed.rsplit_once(':') else {
        return Err(BerthError::InvalidConfig(format!(
            "mgmt_entry `{raw}` must be HOST:PORT"
        )));
    };
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');
    let port: u16 = port_str.parse().map_err(|_| {
        BerthError::InvalidConfig(format!("mgmt_entry `{raw}` has invalid port"))
    })?;
    if host_clean.is_empty() || port == 0 {
        return Err(BerthError::InvalidConfig(format!(
            "mgmt_entry `{raw}` must have a non-empty host and non-zero port"
        )));
    }
    Ok(())
}

/// Atomically write a private file with `0600` permissions on Unix (and `0700`
/// parent directory when created).
pub fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), BerthError> {
    let parent = path.parent().ok_or_else(|| {
        BerthError::Io(format!("path has no parent directory: {}", path.display()))
    })?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("berth");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));

    let write_result = (|| -> io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(BerthError::Io(error.to_string()));
    }

    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(BerthError::Io(error.to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load_config(paths: &BerthPaths) -> Result<Option<BerthConfig>, BerthError> {
    let target = if paths.config_file.is_file() {
        paths.config_file.clone()
    } else if let Some(parent) = paths.config_file.parent().and_then(|p| p.parent()) {
        // Seamless fallback for legacy ~/.config/ags/berth.json installs.
        let legacy = parent.join("ags").join("berth.json");
        if legacy.is_file() {
            legacy
        } else {
            return Ok(None);
        }
    } else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&target)?;
    let config: BerthConfig = serde_json::from_str(&raw)?;
    config.validate()?;
    Ok(Some(config))
}

pub fn save_config(paths: &BerthPaths, config: &BerthConfig) -> Result<(), BerthError> {
    config.validate()?;
    let mut encoded = serde_json::to_vec_pretty(config)?;
    encoded.push(b'\n');
    write_private_file_atomic(&paths.config_file, &encoded)
}

pub fn clear_config(paths: &BerthPaths) -> Result<bool, BerthError> {
    if !paths.config_file.exists() {
        return Ok(false);
    }
    fs::remove_file(&paths.config_file)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Wire Contracts (Berth Central <-> AGS v2 <-> berth-edge)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_preference: Vec<String>,
    #[serde(default = "default_zone_preference")]
    pub zone_preference: String,
    #[serde(default)]
    pub isolation_mode: IsolationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohort_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebind_ref: Option<String>,
    #[serde(default = "default_ags_version")]
    pub ags_version: String,
}

fn de_string_or_int<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or integer, got {other}"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseCageProfile {
    #[serde(deserialize_with = "de_string_or_int")]
    pub cage_id: String,
    #[serde(default = "default_gate_epoch")]
    pub gate_epoch: u64,
    pub timezone: String,
    pub country_code: String,
    pub locale: String,
    #[serde(default)]
    pub accept_language: String,
    #[serde(default)]
    pub asn_class: String,
    #[serde(default)]
    pub strict_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressCandidate {
    #[serde(deserialize_with = "de_string_or_int")]
    pub node_id: String,
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub probe_url: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

impl IngressCandidate {
    pub fn effective_probe_url(&self) -> String {
        let trimmed = self.probe_url.trim();
        if !trimmed.is_empty() {
            trimmed.to_string()
        } else {
            format!("{}/healthz", self.url.trim_end_matches('/'))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseZonePlan {
    #[serde(alias = "zone_id", alias = "zone", alias = "slug")]
    pub zone_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone_source: Option<String>,
    #[serde(default, alias = "ingress_relays", alias = "candidates")]
    pub ingress_candidates: Vec<IngressCandidate>,
    #[serde(default)]
    pub mgmt_entries: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub lease_id: String,
    #[serde(alias = "session_token")]
    pub lease_token: String,
    #[serde(deserialize_with = "de_string_or_int")]
    pub work_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<i64>,
    #[serde(default)]
    pub expires_at: String,
    pub cage: LeaseCageProfile,
    pub zone: LeaseZonePlan,
}

impl fmt::Debug for LeaseResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseResponse")
            .field("lease_id", &self.lease_id)
            .field("lease_token", &"[REDACTED]")
            .field("work_session_id", &self.work_session_id)
            .field("account_id", &self.account_id)
            .field("expires_at", &self.expires_at)
            .field("cage", &self.cage)
            .field("zone", &self.zone)
            .finish()
    }
}

/// Opaque pre-commit rebind handoff written to `rebind_file` (mode `0600`).
/// Intentionally carries NO `lease_token` or reusable secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebindSignal {
    pub rebind_ref: String,
    pub current_cohort_tz: String,
    pub recommended_cohort_tz: String,
    pub recommended_country_code: String,
    pub recommended_locale: String,
    #[serde(default)]
    pub available_cohorts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedIngress {
    pub candidate: IngressCandidate,
    pub rtt_ms: Option<u64>,
    pub probed: bool,
}

// ---------------------------------------------------------------------------
// Minimal Stdlib + Rustls HTTP/1.1 + BHE1 Transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub is_https: bool,
    pub host: String,
    pub port: u16,
    pub path_and_query: String,
}

impl ParsedUrl {
    pub(crate) fn parse(raw: &str) -> Result<Self, BerthError> {
        let trimmed = raw.trim();
        if trimmed.bytes().any(|b| b.is_ascii_control()) {
            return Err(BerthError::InvalidConfig(format!(
                "URL contains control characters: `{raw}`"
            )));
        }
        let (is_https, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(BerthError::InvalidConfig(format!(
                "URL must start with http:// or https://: `{raw}`"
            )));
        };

        let (authority, path_part) = match rest.find(['/', '?']) {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(BerthError::InvalidConfig(format!(
                "invalid URL authority (userinfo not allowed): `{raw}`"
            )));
        }

        let default_port = if is_https { 443 } else { 80 };
        let (host, port) = if let Some(bracket_rest) = authority.strip_prefix('[') {
            let Some((ipv6_host, tail)) = bracket_rest.split_once(']') else {
                return Err(BerthError::InvalidConfig(format!(
                    "unterminated IPv6 literal in URL: `{raw}`"
                )));
            };
            let port = if tail.is_empty() {
                default_port
            } else if let Some(port_str) = tail.strip_prefix(':') {
                port_str.parse::<u16>().map_err(|_| {
                    BerthError::InvalidConfig(format!("invalid port in URL: `{raw}`"))
                })?
            } else {
                return Err(BerthError::InvalidConfig(format!(
                    "invalid IPv6 authority in URL: `{raw}`"
                )));
            };
            (ipv6_host.to_string(), port)
        } else if let Some((h, port_str)) = authority.rsplit_once(':') {
            let port = port_str.parse::<u16>().map_err(|_| {
                BerthError::InvalidConfig(format!("invalid port in URL: `{raw}`"))
            })?;
            (h.to_string(), port)
        } else {
            (authority.to_string(), default_port)
        };

        if host.is_empty() || port == 0 {
            return Err(BerthError::InvalidConfig(format!(
                "invalid host or port in URL: `{raw}`"
            )));
        }

        let path_and_query = if path_part.is_empty() {
            "/".to_string()
        } else if path_part.starts_with('?') {
            format!("/{path_part}")
        } else {
            path_part.to_string()
        };

        Ok(Self {
            is_https,
            host,
            port,
            path_and_query,
        })
    }

    pub(crate) fn host_header(&self) -> String {
        let is_default = (self.is_https && self.port == 443) || (!self.is_https && self.port == 80);
        let formatted_host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if is_default {
            formatted_host
        } else {
            format!("{formatted_host}:{}", self.port)
        }
    }

    pub(crate) fn connect_addr(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub(crate) fn join_path(&self, suffix: &str) -> String {
        let base = self
            .path_and_query
            .split('?')
            .next()
            .unwrap_or("/")
            .trim_end_matches('/');
        if base.is_empty() {
            if suffix.starts_with('/') {
                suffix.to_string()
            } else {
                format!("/{suffix}")
            }
        } else if suffix.starts_with('/') {
            format!("{base}{suffix}")
        } else {
            format!("{base}/{suffix}")
        }
    }
}

pub(crate) fn build_tls_client_config() -> Result<Arc<ClientConfig>, BerthError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Ok(extra_ca_path) = std::env::var("AGS_BERTH_CA_CERT") {
        let trimmed = extra_ca_path.trim();
        if !trimmed.is_empty() {
            let pem_bytes = fs::read(trimmed).map_err(|e| {
                BerthError::Transport(format!("failed to read AGS_BERTH_CA_CERT `{trimmed}`: {e}"))
            })?;
            for cert in CertificateDer::pem_slice_iter(&pem_bytes) {
                let cert = cert.map_err(|e| {
                    BerthError::Transport(format!("invalid PEM in AGS_BERTH_CA_CERT: {e}"))
                })?;
                roots.add(cert).map_err(|e| {
                    BerthError::Transport(format!("failed to add cert from AGS_BERTH_CA_CERT: {e}"))
                })?;
            }
        }
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| BerthError::Transport(format!("TLS protocol config error: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Force HTTP/1.1 ALPN so our HTTP/1.1 framing and WS upgrades work cleanly over TLS.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

pub(crate) enum ClientTransport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for ClientTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for ClientTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl ClientTransport {
    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.get_ref().set_read_timeout(timeout),
        }
    }

    pub(crate) fn connect_direct(url: &ParsedUrl, timeout: Duration) -> Result<Self, BerthError> {
        let tcp = connect_tcp_with_timeout(&url.connect_addr(), timeout)?;
        Self::wrap_for_url(tcp, url)
    }

    pub(crate) fn connect_via_entry(
        entry_addr: &str,
        central_url: &ParsedUrl,
        timeout: Duration,
    ) -> Result<Self, BerthError> {
        let tcp = connect_tcp_with_timeout(entry_addr, timeout)?;
        Self::wrap_for_url(tcp, central_url)
    }

    fn wrap_for_url(tcp: TcpStream, url: &ParsedUrl) -> Result<Self, BerthError> {
        if !url.is_https {
            return Ok(Self::Plain(tcp));
        }
        let tls_config = build_tls_client_config()?;
        let server_name = if let Ok(ip) = url.host.parse::<IpAddr>() {
            ServerName::from(ip)
        } else {
            ServerName::try_from(url.host.clone()).map_err(|e| {
                BerthError::Transport(format!("invalid TLS SNI host `{}`: {e}", url.host))
            })?
        };
        let conn = ClientConnection::new(tls_config, server_name)
            .map_err(|e| BerthError::Transport(format!("TLS init error: {e}")))?;
        Ok(Self::Tls(Box::new(StreamOwned::new(conn, tcp))))
    }
}

pub(crate) fn connect_tcp_with_timeout(addr: &str, timeout: Duration) -> Result<TcpStream, BerthError> {
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .map_err(|e| BerthError::Transport(format!("failed to resolve `{addr}`: {e}")))?
        .collect();
    if resolved.is_empty() {
        return Err(BerthError::Transport(format!(
            "no socket addresses resolved for `{addr}`"
        )));
    }
    let mut last_err = None;
    for sock_addr in resolved {
        match TcpStream::connect_timeout(&sock_addr, timeout) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(timeout));
                let _ = stream.set_write_timeout(Some(timeout));
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(BerthError::Transport(format!(
        "failed to connect to `{addr}`: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".into())
    )))
}

// ---------------------------------------------------------------------------
// BHE1 Attested Management Entry Helpers
// ---------------------------------------------------------------------------

/// Generates a 32-char lowercase hex nonce for `BHE1 attest`.
pub fn generate_bhe1_nonce() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Formats a `BHE1 attest <nonce>\n` preface line.
pub fn format_bhe1_attest_line(nonce: &str) -> Result<String, BerthError> {
    if !(16..=64).contains(&nonce.len())
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(BerthError::InvalidArgument(
            "BHE1 nonce must be 16-64 lowercase hex characters".into(),
        ));
    }
    Ok(format!("BHE1 attest {nonce}\n"))
}

/// Parses a `BHE1 ok <jwt>` or `BHE1 no <reason>` response line.
pub fn parse_bhe1_response(line: &str) -> Result<String, BerthError> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if let Some(token) = trimmed.strip_prefix("BHE1 ok ") {
        let token = token.trim();
        if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(BerthError::Transport(
                "BHE1 gateway returned an empty or malformed attestation token".into(),
            ));
        }
        return Ok(token.to_string());
    }
    if let Some(reason) = trimmed.strip_prefix("BHE1 no ") {
        return Err(BerthError::Transport(format!(
            "BHE1 gateway refused attestation: {}",
            reason.trim()
        )));
    }
    Err(BerthError::Transport(format!(
        "unexpected BHE1 gateway response: `{trimmed}`"
    )))
}

/// Contacts a registered Berth management entry gateway to obtain a short-lived
/// signed `BHE1` client-origin attestation token.
pub fn bhe1_attest(entry_addr: &str, timeout: Duration) -> Result<String, BerthError> {
    validate_host_port(entry_addr)?;
    let nonce = generate_bhe1_nonce();
    let request_line = format_bhe1_attest_line(&nonce)?;
    let mut stream = connect_tcp_with_timeout(entry_addr, timeout)?;
    stream
        .write_all(request_line.as_bytes())
        .map_err(|e| BerthError::Transport(format!("BHE1 write error to `{entry_addr}`: {e}")))?;
    stream
        .flush()
        .map_err(|e| BerthError::Transport(format!("BHE1 flush error to `{entry_addr}`: {e}")))?;

    let mut line = String::new();
    let mut limited = BufReader::new(stream.take(MAX_BHE1_LINE_BYTES as u64));
    let n = limited
        .read_line(&mut line)
        .map_err(|e| BerthError::Transport(format!("BHE1 read error from `{entry_addr}`: {e}")))?;
    if n == 0 {
        return Err(BerthError::Transport(format!(
            "BHE1 gateway `{entry_addr}` closed without responding"
        )));
    }
    parse_bhe1_response(&line)
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn execute_http_once(
    mut stream: ClientTransport,
    method: &str,
    host_header: &str,
    path_and_query: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse, BerthError> {
    let mut req = Vec::with_capacity(512);
    let _ = write!(
        &mut req,
        "{method} {path_and_query} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nUser-Agent: berth/{}\r\n",
        env!("CARGO_PKG_VERSION")
    );
    for (name, value) in headers {
        let _ = write!(&mut req, "{name}: {value}\r\n");
    }
    if let Some(payload) = body {
        let _ = write!(&mut req, "Content-Length: {}\r\n", payload.len());
    }
    req.extend_from_slice(b"\r\n");
    if let Some(payload) = body {
        req.extend_from_slice(payload);
    }

    stream
        .write_all(&req)
        .map_err(|e| BerthError::Transport(format!("HTTP write failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| BerthError::Transport(format!("HTTP flush failed: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    if reader
        .read_line(&mut status_line)
        .map_err(|e| BerthError::Transport(format!("HTTP status read failed: {e}")))?
        == 0
    {
        return Err(BerthError::Transport(
            "upstream closed connection before HTTP status line".into(),
        ));
    }
    let status = parse_http_status_code(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut header_bytes = status_line.len();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| BerthError::Transport(format!("HTTP header read failed: {e}")))?;
        if n == 0 {
            break;
        }
        header_bytes += n;
        if header_bytes > 64 * 1024 {
            return Err(BerthError::Transport("HTTP response headers too large".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let lower = name.trim().to_ascii_lowercase();
            let val = value.trim();
            if lower == "content-length" {
                content_length = val.parse::<usize>().ok();
            } else if lower == "transfer-encoding" && val.to_ascii_lowercase().contains("chunked") {
                is_chunked = true;
            }
        }
    }

    let body_bytes = if method == "HEAD" || status == 204 || status == 304 {
        Vec::new()
    } else if is_chunked {
        read_chunked_body(&mut reader, MAX_HTTP_RESPONSE_BYTES)?
    } else if let Some(len) = content_length {
        if len > MAX_HTTP_RESPONSE_BYTES {
            return Err(BerthError::Transport(format!(
                "HTTP response body exceeds max size ({len} bytes)"
            )));
        }
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| BerthError::Transport(format!("HTTP body read failed: {e}")))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .take((MAX_HTTP_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| BerthError::Transport(format!("HTTP body read failed: {e}")))?;
        if buf.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(BerthError::Transport("HTTP response body too large".into()));
        }
        buf
    };

    Ok(HttpResponse {
        status,
        body: body_bytes,
    })
}

pub(crate) fn parse_http_status_code(status_line: &str) -> Result<u16, BerthError> {
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code_str = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(BerthError::Transport(format!(
            "invalid HTTP status line: `{}`",
            status_line.trim()
        )));
    }
    code_str.parse::<u16>().map_err(|_| {
        BerthError::Transport(format!("invalid HTTP status code in `{}`", status_line.trim()))
    })
}

pub(crate) fn read_chunked_body<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, BerthError> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader
            .read_line(&mut size_line)
            .map_err(|e| BerthError::Transport(format!("chunk size read failed: {e}")))?
            == 0
        {
            break;
        }
        let hex_part = size_line
            .trim()
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|_| {
            BerthError::Transport(format!("invalid chunked encoding size `{hex_part}`"))
        })?;
        if chunk_size == 0 {
            // Drain trailer lines until empty line.
            loop {
                let mut trailer = String::new();
                let n = reader
                    .read_line(&mut trailer)
                    .map_err(|e| BerthError::Transport(format!("chunk trailer read failed: {e}")))?;
                if n == 0 || trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if out.len().saturating_add(chunk_size) > max_bytes {
            return Err(BerthError::Transport(
                "chunked HTTP response exceeds maximum allowed size".into(),
            ));
        }
        let start = out.len();
        out.resize(start + chunk_size, 0);
        reader
            .read_exact(&mut out[start..])
            .map_err(|e| BerthError::Transport(format!("chunk data read failed: {e}")))?;
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| BerthError::Transport(format!("chunk CRLF read failed: {e}")))?;
    }
    Ok(out)
}

fn central_http_request_with_token(
    config: &BerthConfig,
    bearer: Option<&str>,
    method: &str,
    api_path: &str,
    body: Option<&[u8]>,
) -> Result<HttpResponse, BerthError> {
    config.validate_transport()?;
    let central_url = ParsedUrl::parse(&config.central_url)?;
    let full_path = central_url.join_path(api_path);
    let host_header = central_url.host_header();

    let mut base_headers: Vec<(&str, String)> = vec![
        ("Accept", "application/json".to_string()),
        ("X-Berth-AGS-Version", AGS_VERSION_HEADER_VALUE.to_string()),
    ];
    if let Some(token) = bearer {
        let trimmed = token.trim();
        if trimmed.is_empty() || trimmed.contains('\r') || trimmed.contains('\n') {
            return Err(BerthError::InvalidConfig(
                "bearer token must be a non-empty single-line string".into(),
            ));
        }
        base_headers.push(("Authorization", format!("Bearer {trimmed}")));
    }
    if body.is_some() {
        base_headers.push(("Content-Type", "application/json".to_string()));
    }

    let entries_enabled = std::env::var("BERTH_MANAGEMENT_ENTRIES").as_deref() != Ok("0");
    if entries_enabled && !config.mgmt_entries.is_empty() {
        for entry in &config.mgmt_entries {
            if let Ok(attestation) = bhe1_attest(entry, Duration::from_secs(5))
                && let Ok(stream) =
                    ClientTransport::connect_via_entry(entry, &central_url, DEFAULT_HTTP_TIMEOUT)
            {
                let mut entry_headers = base_headers.clone();
                entry_headers.push(("X-Berth-Entry-Attestation", attestation));
                if let Ok(resp) = execute_http_once(
                    stream,
                    method,
                    &host_header,
                    &full_path,
                    &entry_headers,
                    body,
                ) {
                    return Ok(resp);
                }
            }
        }
    }

    let direct_stream = ClientTransport::connect_direct(&central_url, DEFAULT_HTTP_TIMEOUT)?;
    execute_http_once(
        direct_stream,
        method,
        &host_header,
        &full_path,
        &base_headers,
        body,
    )
}

// ---------------------------------------------------------------------------
// Normal-User Authentication & Platform-Scoped API Key Resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BerthUserKeyGroup {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BerthUserKey {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub group: Option<BerthUserKeyGroup>,
}

impl fmt::Debug for BerthUserKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BerthUserKey")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("key", &redact_secret(&self.key))
            .field("status", &self.status)
            .field("group_id", &self.group_id)
            .field("group", &self.group)
            .finish()
    }
}

impl BerthUserKey {
    pub fn platform(&self) -> Option<&str> {
        self.group
            .as_ref()
            .map(|g| g.platform.as_str())
            .filter(|p| !p.trim().is_empty())
    }

    pub fn is_active(&self) -> bool {
        let key_ok = self.status.is_empty() || self.status.eq_ignore_ascii_case("active");
        let group_ok = self
            .group
            .as_ref()
            .map(|g| g.status.is_empty() || g.status.eq_ignore_ascii_case("active"))
            .unwrap_or(true);
        key_ok && group_ok
    }

    pub fn redacted_key(&self) -> String {
        redact_secret(&self.key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    Authenticated(UserAuthSession),
    Requires2FA {
        temp_token: String,
        user_email_masked: String,
    },
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct AuthUserPayload {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    email: String,
    #[serde(default)]
    role: String,
}

#[derive(Deserialize)]
struct AuthLoginData {
    #[serde(default)]
    requires_2fa: bool,
    #[serde(default)]
    temp_token: Option<String>,
    #[serde(default)]
    user_email_masked: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    user: Option<AuthUserPayload>,
}

fn decode_envelope_or_direct<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, BerthError> {
    if let Ok(env) = serde_json::from_slice::<ApiEnvelope<T>>(body) {
        return Ok(env.data);
    }
    Ok(serde_json::from_slice::<T>(body)?)
}

fn build_user_session(
    fallback_email: &str,
    data: AuthLoginData,
) -> Result<UserAuthSession, BerthError> {
    let access_token = data
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| BerthError::Json("auth response missing access_token".into()))?;
    let refresh_token = data.refresh_token.filter(|t| !t.trim().is_empty());
    let expires_at_unix = data
        .expires_in
        .filter(|secs| *secs > 0)
        .map(|secs| unix_timestamp_now().saturating_add(secs));
    let (user_id, email, role) = match data.user {
        Some(u) => (
            u.id,
            if u.email.trim().is_empty() {
                fallback_email.to_string()
            } else {
                u.email
            },
            u.role,
        ),
        None => (0, fallback_email.to_string(), "user".to_string()),
    };
    Ok(UserAuthSession {
        email,
        user_id,
        role,
        access_token,
        refresh_token,
        expires_at_unix,
    })
}

/// Authenticate a normal Berth user via `POST /api/v1/auth/login`.
pub fn login_with_password(
    config: &BerthConfig,
    email: &str,
    password: &str,
) -> Result<LoginOutcome, BerthError> {
    let email_trimmed = email.trim();
    if email_trimmed.is_empty() || password.is_empty() {
        return Err(BerthError::InvalidArgument(
            "email and password must not be empty".into(),
        ));
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "email": email_trimmed,
        "password": password,
    }))?;
    let resp =
        central_http_request_with_token(config, None, "POST", "/api/v1/auth/login", Some(&payload))?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    let data: AuthLoginData = decode_envelope_or_direct(&resp.body)?;
    if data.requires_2fa {
        let temp_token = data
            .temp_token
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| BerthError::Json("2FA response missing temp_token".into()))?;
        return Ok(LoginOutcome::Requires2FA {
            temp_token,
            user_email_masked: data
                .user_email_masked
                .unwrap_or_else(|| email_trimmed.to_string()),
        });
    }
    Ok(LoginOutcome::Authenticated(build_user_session(
        email_trimmed,
        data,
    )?))
}

/// Complete TOTP 2FA login via `POST /api/v1/auth/login/2fa`.
pub fn login_with_2fa(
    config: &BerthConfig,
    fallback_email: &str,
    temp_token: &str,
    totp_code: &str,
) -> Result<UserAuthSession, BerthError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "temp_token": temp_token.trim(),
        "totp_code": totp_code.trim(),
    }))?;
    let resp = central_http_request_with_token(
        config,
        None,
        "POST",
        "/api/v1/auth/login/2fa",
        Some(&payload),
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    let data: AuthLoginData = decode_envelope_or_direct(&resp.body)?;
    build_user_session(fallback_email, data)
}

/// Refresh an access/refresh token pair via `POST /api/v1/auth/refresh`.
pub fn refresh_user_session(
    config: &BerthConfig,
    current: &UserAuthSession,
) -> Result<UserAuthSession, BerthError> {
    let Some(refresh_token) = current.refresh_token.as_deref() else {
        return Err(BerthError::InvalidConfig(
            "saved user session has no refresh_token; please run `bh login` again".into(),
        ));
    };
    let payload = serde_json::to_vec(&serde_json::json!({
        "refresh_token": refresh_token,
    }))?;
    let resp = central_http_request_with_token(
        config,
        None,
        "POST",
        "/api/v1/auth/refresh",
        Some(&payload),
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    let data: AuthLoginData = decode_envelope_or_direct(&resp.body)?;
    let access_token = data
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| BerthError::Json("refresh response missing access_token".into()))?;
    let new_refresh = data
        .refresh_token
        .filter(|t| !t.trim().is_empty())
        .or_else(|| current.refresh_token.clone());
    let expires_at_unix = data
        .expires_in
        .filter(|secs| *secs > 0)
        .map(|secs| unix_timestamp_now().saturating_add(secs))
        .or(current.expires_at_unix);
    Ok(UserAuthSession {
        email: current.email.clone(),
        user_id: current.user_id,
        role: current.role.clone(),
        access_token,
        refresh_token: new_refresh,
        expires_at_unix,
    })
}

/// Ensures `config.user_session` is fresh; automatically refreshes and persists
/// when within 60 seconds of expiry.
pub fn ensure_fresh_user_session(
    paths: &BerthPaths,
    config: &mut BerthConfig,
) -> Result<Option<UserAuthSession>, BerthError> {
    let Some(current) = config.user_session.clone() else {
        return Ok(None);
    };
    if current.is_expiring_within(60) && current.refresh_token.is_some() {
        let refreshed = refresh_user_session(config, &current)?;
        config.user_session = Some(refreshed.clone());
        let _ = save_config(paths, config);
        return Ok(Some(refreshed));
    }
    Ok(Some(current))
}

/// Revokes the active refresh token via `POST /api/v1/auth/logout`.
pub fn logout_user_session(
    config: &BerthConfig,
    session: &UserAuthSession,
) -> Result<(), BerthError> {
    let payload = match session.refresh_token.as_deref() {
        Some(rt) => serde_json::to_vec(&serde_json::json!({ "refresh_token": rt }))?,
        None => b"{}".to_vec(),
    };
    let resp = central_http_request_with_token(
        config,
        Some(&session.access_token),
        "POST",
        "/api/v1/auth/logout",
        Some(&payload),
    )?;
    if (200..300).contains(&resp.status) || resp.status == 401 {
        return Ok(());
    }
    Err(BerthError::ApiStatus {
        status: resp.status,
        body: String::from_utf8_lossy(&resp.body).into_owned(),
    })
}

#[derive(Deserialize)]
struct KeyListData {
    #[serde(default)]
    items: Vec<BerthUserKey>,
}

/// Fetches the authenticated user's active API keys via `GET /api/v1/keys`.
pub fn list_user_api_keys(
    config: &BerthConfig,
    access_token: &str,
) -> Result<Vec<BerthUserKey>, BerthError> {
    let resp = central_http_request_with_token(
        config,
        Some(access_token),
        "GET",
        "/api/v1/keys?status=active&page=1&page_size=100",
        None,
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    let data: KeyListData = decode_envelope_or_direct(&resp.body)?;
    Ok(data.items)
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedAgentAuth {
    pub bearer_token: String,
    pub source: String,
    pub platform: String,
    pub key_name: Option<String>,
    pub key_id: Option<i64>,
    pub group_name: Option<String>,
}

impl fmt::Debug for ResolvedAgentAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedAgentAuth")
            .field("bearer_token", &redact_secret(&self.bearer_token))
            .field("source", &self.source)
            .field("platform", &self.platform)
            .field("key_name", &self.key_name)
            .field("key_id", &self.key_id)
            .field("group_name", &self.group_name)
            .finish()
    }
}

impl ResolvedAgentAuth {
    pub fn token_fingerprint(&self) -> String {
        redact_secret(&self.bearer_token)
    }
}

fn select_matching_platform_key<'a>(
    keys: &'a [BerthUserKey],
    canonical_agent: &str,
    platform: &str,
) -> Option<(&'a BerthUserKey, &'static str)> {
    let active: Vec<&BerthUserKey> = keys.iter().filter(|k| k.is_active()).collect();

    // 1. Exact group platform match (e.g., codex -> openai, claude -> anthropic, agy -> antigravity, grok -> xai).
    if let Some(found) = active.iter().find(|k| {
        k.platform()
            .is_some_and(|p| p.eq_ignore_ascii_case(platform))
    }) {
        return Some((*found, "auto-platform-key"));
    }

    // 2. Platform aliases: `agy` also accepts `gemini`; `grok` also accepts `xai`.
    if canonical_agent == "agy"
        && let Some(found) = active.iter().find(|k| {
            k.platform()
                .is_some_and(|p| p.eq_ignore_ascii_case("gemini"))
        })
    {
        return Some((*found, "auto-platform-key"));
    }
    if canonical_agent == "grok"
        && let Some(found) = active.iter().find(|k| {
            k.platform()
                .is_some_and(|p| p.eq_ignore_ascii_case("xai"))
        })
    {
        return Some((*found, "auto-platform-key"));
    }

    // 3. Fallback to an active ungrouped key (`group_id == None`).
    if let Some(found) = active
        .iter()
        .find(|k| k.group_id.is_none() && k.group.is_none())
    {
        return Some((*found, "auto-ungrouped-key"));
    }

    None
}

/// Resolves the exact Bearer credential (`sk-...` or user JWT) to authenticate
/// lease minting for `agent`:
/// 1. Explicit `--key` (`sk-...` directly, or matched by key name/ID via `/api/v1/keys`)
/// 2. Local pinned override from `bh key set <agent|platform> <sk-...>`
/// 3. Logged-in `user_session` (auto-refreshed if needed) -> automatic per-platform `sk-...`
///    selection from `GET /api/v1/keys?status=active` (or ungrouped active key, else user JWT)
/// 4. Default static `config.api_key`
pub fn resolve_auth_for_agent(
    paths: &BerthPaths,
    config: &mut BerthConfig,
    agent: &str,
    explicit_key: Option<&str>,
) -> Result<ResolvedAgentAuth, BerthError> {
    let canonical_agent = validate_agent_name(agent)?;
    let platform = agent_to_platform(canonical_agent)?.to_string();

    // 1. Explicit one-shot `--key` override.
    if let Some(raw) = explicit_key.map(str::trim).filter(|s| !s.is_empty()) {
        if raw.starts_with("sk-") || config.user_session.is_none() {
            return Ok(ResolvedAgentAuth {
                bearer_token: raw.to_string(),
                source: "explicit-flag".to_string(),
                platform,
                key_name: None,
                key_id: None,
                group_name: None,
            });
        }
        if let Some(session) = ensure_fresh_user_session(paths, config)? {
            let keys = list_user_api_keys(config, &session.access_token)?;
            let parsed_id = raw.parse::<i64>().ok();
            if let Some(found) = keys.iter().find(|k| {
                k.is_active()
                    && (k.name.eq_ignore_ascii_case(raw)
                        || parsed_id.is_some_and(|id| k.id == id))
            }) {
                return Ok(ResolvedAgentAuth {
                    bearer_token: found.key.clone(),
                    source: "explicit-key-name".to_string(),
                    platform,
                    key_name: Some(found.name.clone()),
                    key_id: Some(found.id),
                    group_name: found.group.as_ref().map(|g| g.name.clone()),
                });
            }
            let available: Vec<String> = keys
                .iter()
                .filter(|k| k.is_active())
                .map(|k| format!("{} (id={})", k.name, k.id))
                .collect();
            return Err(BerthError::InvalidArgument(format!(
                "no active API key matching `{raw}` found on your Berth account (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )));
        }
    }

    // 2. Local pinned per-agent / per-platform override (`bh key set`).
    if let Some(pinned) = config
        .platform_keys
        .get(canonical_agent)
        .or_else(|| config.platform_keys.get(&platform))
        .filter(|k| !k.trim().is_empty())
    {
        return Ok(ResolvedAgentAuth {
            bearer_token: pinned.clone(),
            source: "pinned-platform-key".to_string(),
            platform,
            key_name: None,
            key_id: None,
            group_name: None,
        });
    }

    // 3. Logged-in user session -> auto-select matching platform `sk-...` key.
    if let Some(session) = ensure_fresh_user_session(paths, config)? {
        let mut keys_result = list_user_api_keys(config, &session.access_token);
        let mut active_session = session;

        // If access_token was rejected with 401 and we have a refresh_token, refresh once and retry.
        if let Err(BerthError::ApiStatus { status: 401, .. }) = &keys_result
            && active_session.refresh_token.is_some()
            && let Ok(refreshed) = refresh_user_session(config, &active_session)
        {
            config.user_session = Some(refreshed.clone());
            let _ = save_config(paths, config);
            active_session = refreshed;
            keys_result = list_user_api_keys(config, &active_session.access_token);
        }

        if let Ok(keys) = keys_result
            && let Some((matched, source)) =
                select_matching_platform_key(&keys, canonical_agent, &platform)
        {
            return Ok(ResolvedAgentAuth {
                bearer_token: matched.key.clone(),
                source: source.to_string(),
                platform,
                key_name: Some(matched.name.clone()),
                key_id: Some(matched.id),
                group_name: matched.group.as_ref().map(|g| g.name.clone()),
            });
        }

        if !config.api_key.trim().is_empty() {
            return Ok(ResolvedAgentAuth {
                bearer_token: config.api_key.clone(),
                source: "default-api-key".to_string(),
                platform,
                key_name: None,
                key_id: None,
                group_name: None,
            });
        }

        return Ok(ResolvedAgentAuth {
            bearer_token: active_session.access_token,
            source: "user-jwt-fallback".to_string(),
            platform,
            key_name: None,
            key_id: None,
            group_name: None,
        });
    }

    // 4. Static default `config.api_key`.
    if !config.api_key.trim().is_empty() {
        return Ok(ResolvedAgentAuth {
            bearer_token: config.api_key.clone(),
            source: "default-api-key".to_string(),
            platform,
            key_name: None,
            key_id: None,
            group_name: None,
        });
    }

    Err(BerthError::NotConfigured)
}

fn decode_lease_response(body: &[u8]) -> Result<LeaseResponse, BerthError> {
    if let Ok(direct) = serde_json::from_slice::<LeaseResponse>(body) {
        return Ok(direct);
    }
    #[derive(Deserialize)]
    struct Envelope {
        data: LeaseResponse,
    }
    let env: Envelope = serde_json::from_slice(body)?;
    Ok(env.data)
}

/// Acquire a Berth v2 Cage Session Lease via `POST /api/v1/ags/leases` with an explicit Bearer token.
pub fn acquire_lease_with_token(
    config: &BerthConfig,
    bearer_token: &str,
    req: &LeaseRequest,
) -> Result<LeaseResponse, BerthError> {
    let _ = validate_agent_name(&req.agent)?;
    let payload = serde_json::to_vec(req)?;
    let resp = central_http_request_with_token(
        config,
        Some(bearer_token),
        "POST",
        "/api/v1/ags/leases",
        Some(&payload),
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    decode_lease_response(&resp.body)
}

/// Acquire a Berth v2 Cage Session Lease via `POST /api/v1/ags/leases` using `config.api_key`.
pub fn acquire_lease(
    config: &BerthConfig,
    req: &LeaseRequest,
) -> Result<LeaseResponse, BerthError> {
    config.validate()?;
    acquire_lease_with_token(config, &config.api_key, req)
}

/// Renew an active Berth v2 Lease via `POST /api/v1/ags/leases/{lease_id}/renew` with an explicit Bearer token.
pub fn renew_lease_with_token(
    config: &BerthConfig,
    bearer_token: &str,
    lease_id: &str,
) -> Result<LeaseResponse, BerthError> {
    let encoded_id = urlencoding::encode(lease_id);
    let path = format!("/api/v1/ags/leases/{encoded_id}/renew");
    let resp =
        central_http_request_with_token(config, Some(bearer_token), "POST", &path, Some(b"{}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(BerthError::ApiStatus {
            status: resp.status,
            body: String::from_utf8_lossy(&resp.body).into_owned(),
        });
    }
    decode_lease_response(&resp.body)
}

/// Renew an active Berth v2 Lease via `POST /api/v1/ags/leases/{lease_id}/renew`.
pub fn renew_lease(config: &BerthConfig, lease_id: &str) -> Result<LeaseResponse, BerthError> {
    config.validate()?;
    renew_lease_with_token(config, &config.api_key, lease_id)
}

/// Release an active Berth v2 Lease via `DELETE /api/v1/ags/leases/{lease_id}` with an explicit Bearer token.
pub fn release_lease_with_token(
    config: &BerthConfig,
    bearer_token: &str,
    lease_id: &str,
) -> Result<(), BerthError> {
    let encoded_id = urlencoding::encode(lease_id);
    let path = format!("/api/v1/ags/leases/{encoded_id}");
    let resp =
        central_http_request_with_token(config, Some(bearer_token), "DELETE", &path, None)?;
    if (200..300).contains(&resp.status) || resp.status == 404 {
        return Ok(());
    }
    Err(BerthError::ApiStatus {
        status: resp.status,
        body: String::from_utf8_lossy(&resp.body).into_owned(),
    })
}

/// Release an active Berth v2 Lease via `DELETE /api/v1/ags/leases/{lease_id}`.
pub fn release_lease(config: &BerthConfig, lease_id: &str) -> Result<(), BerthError> {
    config.validate()?;
    release_lease_with_token(config, &config.api_key, lease_id)
}

fn probe_ingress_once(probe_url: &str, timeout: Duration) -> Option<Duration> {
    let parsed = ParsedUrl::parse(probe_url).ok()?;
    let start = Instant::now();
    let stream = ClientTransport::connect_direct(&parsed, timeout).ok()?;
    let resp = execute_http_once(
        stream,
        "GET",
        &parsed.host_header(),
        &parsed.path_and_query,
        &[("Accept", "*/*".to_string())],
        None,
    )
    .ok()?;
    if (200..300).contains(&resp.status) {
        Some(start.elapsed())
    } else {
        None
    }
}

/// Concurrently probes every ingress candidate's `probe_url` (1500ms timeout),
/// choosing the lowest RTT (tie-breaking by highest `weight`), and falling back
/// gracefully to `candidates[0]` if all probes fail in offline/mock setups.
pub fn select_best_ingress(candidates: &[IngressCandidate]) -> Result<SelectedIngress, BerthError> {
    if candidates.is_empty() {
        return Err(BerthError::InvalidConfig(
            "lease returned zero ingress candidates".into(),
        ));
    }

    let results: Vec<(usize, Option<Duration>)> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(candidates.len());
        for (idx, candidate) in candidates.iter().enumerate() {
            let probe_url = candidate.effective_probe_url();
            handles.push(scope.spawn(move || {
                let elapsed = probe_ingress_once(&probe_url, DEFAULT_PROBE_TIMEOUT);
                (idx, elapsed)
            }));
        }
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    let mut healthy: Vec<(usize, Duration, u32)> = results
        .into_iter()
        .filter_map(|(idx, rtt)| rtt.map(|d| (idx, d, candidates[idx].weight)))
        .collect();

    if healthy.is_empty() {
        return Ok(SelectedIngress {
            candidate: candidates[0].clone(),
            rtt_ms: None,
            probed: false,
        });
    }

    // Lowest RTT (millisecond bucket) wins; tie-break by highest weight,
    // then sub-ms precision, then candidate order.
    healthy.sort_by(|a, b| {
        let a_ms = a.1.as_millis();
        let b_ms = b.1.as_millis();
        a_ms.cmp(&b_ms)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let (best_idx, best_rtt, _) = healthy[0];
    Ok(SelectedIngress {
        candidate: candidates[best_idx].clone(),
        rtt_ms: Some(best_rtt.as_millis() as u64),
        probed: true,
    })
}

// ---------------------------------------------------------------------------
// Explicit Isolation Mode Enforcement (Blocker #5.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIsolationCapabilities {
    pub os: String,
    pub is_wsl: bool,
    pub wsl_interop_active: bool,
    pub zoneinfo_exists: bool,
    pub unshare_user_mount_works: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationReport {
    pub mode: String,
    pub os_egress_isolation: bool,
    pub mount_namespace_localtime: bool,
    pub windows_interop_path_scrubbed: bool,
    pub strict_eligible: bool,
    pub strict_blockers: Vec<String>,
    pub host_visible_notes: Vec<String>,
}

pub fn probe_host_isolation_capabilities(timezone: Option<&str>) -> HostIsolationCapabilities {
    let os = std::env::consts::OS.to_string();
    let is_linux = os == "linux";

    let proc_version = if is_linux {
        fs::read_to_string("/proc/version").unwrap_or_default()
    } else {
        String::new()
    };
    let osrelease = if is_linux {
        fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default()
    } else {
        String::new()
    };
    let is_wsl = is_linux
        && (proc_version.to_ascii_lowercase().contains("microsoft")
            || proc_version.to_ascii_lowercase().contains("wsl")
            || osrelease.to_ascii_lowercase().contains("microsoft")
            || osrelease.to_ascii_lowercase().contains("wsl")
            || std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some());

    let binfmt_interop_enabled = [
        "/proc/sys/fs/binfmt_misc/WSLInterop",
        "/proc/sys/fs/binfmt_misc/WSLInterop-late",
    ]
    .iter()
    .any(|path| {
        fs::read_to_string(path)
            .map(|text| text.lines().any(|line| line.trim() == "enabled"))
            .unwrap_or(false)
    });
    let wsl_interop_active =
        is_linux && (binfmt_interop_enabled || std::env::var_os("WSL_INTEROP").is_some());

    let tz_to_check = timezone.unwrap_or("UTC");
    let zoneinfo_exists = is_linux
        && validate_timezone_identifier(tz_to_check).is_ok()
        && Path::new("/usr/share/zoneinfo").join(tz_to_check).is_file();

    let unshare_user_mount_works = is_linux
        && Command::new("unshare")
            .args(["-Ur", "-m", "true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    HostIsolationCapabilities {
        os,
        is_wsl,
        wsl_interop_active,
        zoneinfo_exists,
        unshare_user_mount_works,
    }
}

pub fn evaluate_isolation(
    mode: IsolationMode,
    timezone: &str,
    caps: &HostIsolationCapabilities,
) -> Result<IsolationReport, BerthError> {
    let mut blockers = Vec::new();
    if caps.os != "linux" {
        blockers.push(format!(
            "host OS is `{}` (strict isolation requires native Linux)",
            caps.os
        ));
    }
    if !caps.zoneinfo_exists {
        blockers.push(format!("/usr/share/zoneinfo/{timezone} does not exist"));
    }
    if caps.is_wsl {
        blockers.push(
            "WSL2 host detected (Windows host egress and interop cannot be strictly confined)"
                .into(),
        );
    }
    if caps.wsl_interop_active {
        blockers.push("active Windows interop (WSLInterop) bypass detected".into());
    }
    if !caps.unshare_user_mount_works {
        blockers.push("`unshare -Ur -m true` failed (user+mount namespaces unavailable)".into());
    }

    let strict_eligible = blockers.is_empty();
    match mode {
        IsolationMode::Strict => {
            if !strict_eligible {
                return Err(BerthError::StrictIsolationUnavailable(format!(
                    "{}; re-run with `--isolation tz-overlay` when OS-level strict controls are unavailable",
                    blockers.join("; ")
                )));
            }
            Ok(IsolationReport {
                mode: "strict".to_string(),
                os_egress_isolation: true,
                mount_namespace_localtime: true,
                windows_interop_path_scrubbed: true,
                strict_eligible: true,
                strict_blockers: Vec::new(),
                host_visible_notes: Vec::new(),
            })
        }
        IsolationMode::TzOverlay => Ok(IsolationReport {
            mode: "tz-overlay".to_string(),
            os_egress_isolation: false,
            mount_namespace_localtime: false,
            windows_interop_path_scrubbed: true,
            strict_eligible,
            strict_blockers: blockers,
            host_visible_notes: vec![
                "tz-overlay aligns TZ/LANG/LC_ALL, overlay hooks, and scrubbed PATH only; it does NOT enforce OS-level network egress isolation".into(),
                "raw /etc/localtime reads and direct non-proxy syscalls remain host-visible unless running in strict Linux mode".into(),
            ],
        }),
    }
}

// ---------------------------------------------------------------------------
// Per-CLI Environment Plan Generator (Stock codex / claude / agy / grok)
// ---------------------------------------------------------------------------

/// Strips `/mnt/<drive>` and `/mnt/<drive>/...` Windows DrvFs segments from `PATH`.
pub fn scrub_windows_interop_path(raw_path: &str) -> String {
    let kept: Vec<&str> = raw_path
        .split(':')
        .filter(|segment| !segment.is_empty() && !is_windows_drvfs_segment(segment))
        .collect();
    if kept.is_empty() {
        "/usr/local/bin:/usr/bin:/bin".to_string()
    } else {
        kept.join(":")
    }
}

fn is_windows_drvfs_segment(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix("/mnt/") else {
        return false;
    };
    let bytes = rest.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    bytes.len() == 1 || bytes[1] == b'/'
}

/// Generates the Codex `[shell_environment_policy]` TOML snippet so Codex tool
/// subshells and MCP stdio servers retain the authoritative Cage TZ/locale.
pub fn build_codex_shell_env_policy_toml(cage: &LeaseCageProfile) -> String {
    format!(
        "[shell_environment_policy]\ninherit = \"core\"\n\n[shell_environment_policy.set]\nTZ = {}\nLANG = {}\nLC_ALL = {}\nLC_TIME = {}\n",
        toml_quote(&cage.timezone),
        toml_quote(&cage.locale),
        toml_quote(&cage.locale),
        toml_quote(&cage.locale),
    )
}

fn toml_quote(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

/// Complete environment + isolation plan for launching a stock CLI against the
/// authenticated loopback `CageShim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliLaunchEnv {
    pub agent: String,
    pub isolation: IsolationReport,
    pub set_env: BTreeMap<String, String>,
    pub unset_env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_shell_env_policy_toml: Option<String>,
    pub scrubbed_path: String,
}

impl CliLaunchEnv {
    pub fn apply_to_command(&self, cmd: &mut Command) {
        for key in &self.unset_env {
            cmd.env_remove(key);
        }
        cmd.env("PATH", &self.scrubbed_path);
        for (key, value) in &self.set_env {
            cmd.env(key, value);
        }
    }

    pub fn to_shell(&self) -> String {
        let mut lines = vec![
            "# ags-berth-v2-env".to_string(),
            format!("# agent={} mode={}", self.agent, self.isolation.mode),
        ];
        for key in &self.unset_env {
            lines.push(format!("unset {key}"));
        }
        lines.push(format!(
            "export PATH={}",
            posix_single_quote(&self.scrubbed_path)
        ));
        for (key, value) in &self.set_env {
            lines.push(format!("export {key}={}", posix_single_quote(value)));
        }
        lines.join("\n") + "\n"
    }
}

pub fn build_cli_launch_env(
    agent: &str,
    shim_url: &str,
    shim_token: &str,
    cage: &LeaseCageProfile,
    isolation_mode: IsolationMode,
) -> Result<CliLaunchEnv, BerthError> {
    let caps = probe_host_isolation_capabilities(Some(&cage.timezone));
    let current_path = std::env::var("PATH").ok();
    build_cli_launch_env_with_caps(
        agent,
        shim_url,
        shim_token,
        cage,
        isolation_mode,
        &caps,
        current_path.as_deref(),
    )
}

pub fn build_cli_launch_env_with_caps(
    agent: &str,
    shim_url: &str,
    shim_token: &str,
    cage: &LeaseCageProfile,
    isolation_mode: IsolationMode,
    caps: &HostIsolationCapabilities,
    current_path: Option<&str>,
) -> Result<CliLaunchEnv, BerthError> {
    let canonical_agent = validate_agent_name(agent)?;
    validate_timezone_identifier(&cage.timezone)?;
    if shim_token.trim().is_empty() {
        return Err(BerthError::InvalidArgument(
            "shim_token must not be empty".into(),
        ));
    }
    let parsed_shim = ParsedUrl::parse(shim_url)?;
    if parsed_shim.host != "127.0.0.1" && !parsed_shim.host.eq_ignore_ascii_case("localhost") {
        return Err(BerthError::InvalidArgument(format!(
            "shim_url must target loopback (127.0.0.1 or localhost); got `{shim_url}`"
        )));
    }

    let isolation = evaluate_isolation(isolation_mode, &cage.timezone, caps)?;
    let base = shim_url.trim_end_matches('/');

    let mut set_env = BTreeMap::new();
    set_env.insert("TZ".to_string(), cage.timezone.clone());
    set_env.insert("LANG".to_string(), cage.locale.clone());
    set_env.insert("LC_ALL".to_string(), cage.locale.clone());
    set_env.insert("LC_TIME".to_string(), cage.locale.clone());
    set_env.insert("AGS_BERTH_CAGE_TZ".to_string(), cage.timezone.clone());
    set_env.insert(
        "AGS_BERTH_CAGE_COUNTRY".to_string(),
        cage.country_code.to_ascii_uppercase(),
    );
    set_env.insert("AGS_BERTH_CAGE_LOCALE".to_string(), cage.locale.clone());
    set_env.insert(
        "AGS_BERTH_ISOLATION_MODE".to_string(),
        isolation.mode.clone(),
    );

    let mut codex_shell_env_policy_toml = None;
    match canonical_agent {
        "codex" => {
            set_env.insert("OPENAI_BASE_URL".to_string(), format!("{base}/v1"));
            set_env.insert("OPENAI_API_KEY".to_string(), shim_token.to_string());
            codex_shell_env_policy_toml = Some(build_codex_shell_env_policy_toml(cage));
        }
        "claude" => {
            set_env.insert("ANTHROPIC_BASE_URL".to_string(), base.to_string());
            set_env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), shim_token.to_string());
            set_env.insert(
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                "1".to_string(),
            );
            set_env.insert("DISABLE_TELEMETRY".to_string(), "1".to_string());
        }
        "agy" => {
            set_env.insert("GEMINI_API_BASE".to_string(), base.to_string());
            set_env.insert("GOOGLE_GEMINI_BASE_URL".to_string(), base.to_string());
            set_env.insert("GEMINI_API_KEY".to_string(), shim_token.to_string());
        }
        "grok" => {
            set_env.insert("XAI_BASE_URL".to_string(), format!("{base}/v1"));
            set_env.insert("GROK_BASE_URL".to_string(), format!("{base}/v1"));
            set_env.insert("XAI_API_KEY".to_string(), shim_token.to_string());
            set_env.insert("GROK_API_KEY".to_string(), shim_token.to_string());
        }
        _ => unreachable!(),
    }

    let mut unset_env: Vec<String> = Vec::new();
    for key in PROXY_ENV_UNSET
        .iter()
        .chain(CARRIER_ENV_UNSET.iter())
        .chain(ALL_PROVIDER_CREDENTIAL_ENVS.iter())
    {
        if !set_env.contains_key(*key) {
            unset_env.push((*key).to_string());
        }
    }
    unset_env.sort();
    unset_env.dedup();

    let scrubbed_path = scrub_windows_interop_path(
        current_path.unwrap_or("/usr/local/bin:/usr/bin:/bin"),
    );

    Ok(CliLaunchEnv {
        agent: canonical_agent.to_string(),
        isolation,
        set_env,
        unset_env,
        codex_shell_env_policy_toml,
        scrubbed_path,
    })
}

/// Resolves the unmodified official stock CLI binary (`codex`, `claude`, `agy`, `grok`).
/// Intentionally NEVER resolves or prefers `codext`.
pub fn resolve_stock_cli_binary(agent: &str) -> Result<PathBuf, BerthError> {
    let canonical = validate_agent_name(agent)?;
    let env_override = match canonical {
        "codex" => "AGENT_SESSION_CODEX_BINARY",
        "claude" => "AGENT_SESSION_CLAUDE_BINARY",
        "agy" => "AGENT_SESSION_AGY_BINARY",
        "grok" => "AGENT_SESSION_GROK_BINARY",
        _ => unreachable!(),
    };
    if let Ok(explicit) = std::env::var(env_override) {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Ok(path);
            }
            return Err(BerthError::InvalidArgument(format!(
                "{env_override} does not point to an executable file: {trimmed}"
            )));
        }
    }
    which::which(canonical).map_err(|_| {
        BerthError::InvalidArgument(format!(
            "cannot find unmodified `{canonical}` binary on PATH (codext is retired)"
        ))
    })
}

/// Writes the Codex `[shell_environment_policy]` into an overlay `CODEX_HOME/config.toml`
/// without ever mutating the user's real `~/.codex/config.toml` (unlinks symlink first).
pub fn write_codex_overlay_config_toml(
    overlay_codex_home: &Path,
    snippet: &str,
) -> Result<PathBuf, BerthError> {
    fs::create_dir_all(overlay_codex_home)?;
    let config_path = overlay_codex_home.join("config.toml");
    let existing = if config_path.is_file() {
        fs::read_to_string(&config_path).unwrap_or_default()
    } else {
        String::new()
    };
    if config_path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::remove_file(&config_path)?;
    }
    // Remove symlinked auth.json inside the overlay only (never touches real ~/.codex/auth.json)
    // so stock `codex` authenticates strictly through the Berth loopback shim.
    let overlay_auth = overlay_codex_home.join("auth.json");
    if overlay_auth
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = fs::remove_file(&overlay_auth);
    }
    let cleaned = strip_toml_table(&existing, "shell_environment_policy");
    let merged = if cleaned.trim().is_empty() {
        snippet.to_string()
    } else {
        format!("{}\n\n{snippet}", cleaned.trim_end())
    };
    write_private_file_atomic(&config_path, merged.as_bytes())?;
    Ok(config_path)
}

fn strip_toml_table(source: &str, table_prefix: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let header = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            skipping = header == table_prefix
                || header.starts_with(&format!("{table_prefix}."));
            if skipping {
                continue;
            }
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join("\n")
}

pub fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn sample_cage(tz: &str, cc: &str, locale: &str) -> LeaseCageProfile {
        LeaseCageProfile {
            cage_id: "cage_tokyo_01".into(),
            gate_epoch: 7,
            timezone: tz.into(),
            country_code: cc.into(),
            locale: locale.into(),
            accept_language: "ja-JP,ja;q=0.9,en-US;q=0.8".into(),
            asn_class: "residential".into(),
            strict_mode: true,
        }
    }

    #[test]
    fn config_roundtrip_atomic_0600_and_https_guard() {
        let dir = TempDir::new().unwrap();
        let paths = BerthPaths::new(dir.path().join("ags/berth.json"), dir.path().join("state"));

        // Plain HTTP is refused by default.
        let insecure = BerthConfig {
            schema: BERTH_CONFIG_SCHEMA.into(),
            central_url: "http://127.0.0.1:8080".into(),
            api_key: "sk-berth-secret-123".into(),
            platform_keys: BTreeMap::new(),
            user_session: None,
            mgmt_entries: vec![],
            zone_preference: "auto".into(),
            isolation_mode: IsolationMode::TzOverlay,
            allow_insecure_http_for_tests: false,
        };
        assert!(save_config(&paths, &insecure).is_err());

        // Valid HTTPS config saves atomically with 0600 permissions.
        let valid = BerthConfig {
            central_url: "https://berth.example.com".into(),
            mgmt_entries: vec!["203.0.113.10:18443".into()],
            zone_preference: "cn_mainland".into(),
            ..insecure
        };
        save_config(&paths, &valid).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.config_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let loaded = load_config(&paths).unwrap().unwrap();
        assert_eq!(loaded, valid);
        assert!(!format!("{loaded:?}").contains("sk-berth-secret-123"));
        assert!(clear_config(&paths).unwrap());
        assert!(load_config(&paths).unwrap().is_none());
    }

    #[test]
    fn bhe1_framing_and_parsing() {
        let nonce = generate_bhe1_nonce();
        assert_eq!(nonce.len(), 32);
        let line = format_bhe1_attest_line(&nonce).unwrap();
        assert_eq!(line, format!("BHE1 attest {nonce}\n"));
        assert!(format_bhe1_attest_line("TOO_SHORT").is_err());
        assert!(format_bhe1_attest_line("UPPERCASE0123456789ABCDEF").is_err());

        assert_eq!(
            parse_bhe1_response("BHE1 ok eyJhbGciOiJIUzI1NiJ9.payload.sig\r\n").unwrap(),
            "eyJhbGciOiJIUzI1NiJ9.payload.sig"
        );
        assert!(parse_bhe1_response("BHE1 no rate_limited\n").is_err());
        assert!(parse_bhe1_response("GARBAGE\n").is_err());
    }

    #[test]
    fn cli_launch_env_fixtures_for_all_four_stock_clis() {
        let cage = sample_cage("Asia/Tokyo", "JP", "ja_JP.UTF-8");
        let caps = HostIsolationCapabilities {
            os: "linux".into(),
            is_wsl: true,
            wsl_interop_active: true,
            zoneinfo_exists: true,
            unshare_user_mount_works: true,
        };
        let dirty_path = "/usr/local/bin:/mnt/c/Windows/System32:/usr/bin:/mnt/D/Tools:/bin";
        let shim_url = "http://127.0.0.1:41234";
        let shim_token = "ags-shim-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // 1. Codex
        let codex = build_cli_launch_env_with_caps(
            "codex",
            shim_url,
            shim_token,
            &cage,
            IsolationMode::TzOverlay,
            &caps,
            Some(dirty_path),
        )
        .unwrap();
        assert_eq!(codex.isolation.mode, "tz-overlay");
        assert!(!codex.isolation.os_egress_isolation);
        assert_eq!(codex.scrubbed_path, "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(codex.set_env["TZ"], "Asia/Tokyo");
        assert_eq!(codex.set_env["LANG"], "ja_JP.UTF-8");
        assert_eq!(codex.set_env["LC_ALL"], "ja_JP.UTF-8");
        assert_eq!(codex.set_env["LC_TIME"], "ja_JP.UTF-8");
        assert_eq!(codex.set_env["OPENAI_BASE_URL"], "http://127.0.0.1:41234/v1");
        assert_eq!(codex.set_env["OPENAI_API_KEY"], shim_token);
        assert!(codex.unset_env.iter().any(|k| k == "HTTPS_PROXY"));
        assert!(codex.unset_env.iter().any(|k| k == "http_proxy"));
        assert!(codex.unset_env.iter().any(|k| k == "NO_PROXY"));
        assert!(codex.unset_env.iter().any(|k| k == "CODEXT_POOL_ACCOUNT"));
        assert!(codex.unset_env.iter().any(|k| k == "ANTHROPIC_API_KEY"));
        let toml = codex.codex_shell_env_policy_toml.as_deref().unwrap();
        assert!(toml.contains("[shell_environment_policy]"));
        assert!(toml.contains("TZ = \"Asia/Tokyo\""));
        assert!(toml.contains("LC_ALL = \"ja_JP.UTF-8\""));

        // 2. Claude
        let claude = build_cli_launch_env_with_caps(
            "claude",
            shim_url,
            shim_token,
            &cage,
            IsolationMode::TzOverlay,
            &caps,
            Some(dirty_path),
        )
        .unwrap();
        assert_eq!(claude.set_env["ANTHROPIC_BASE_URL"], shim_url);
        assert_eq!(claude.set_env["ANTHROPIC_AUTH_TOKEN"], shim_token);
        assert!(claude.unset_env.iter().any(|k| k == "ANTHROPIC_API_KEY"));
        assert!(claude.unset_env.iter().any(|k| k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(claude.unset_env.iter().any(|k| k == "ALL_PROXY"));
        assert!(claude.codex_shell_env_policy_toml.is_none());

        // 3. Agy (Antigravity / Gemini)
        let agy = build_cli_launch_env_with_caps(
            "agy",
            shim_url,
            shim_token,
            &cage,
            IsolationMode::TzOverlay,
            &caps,
            Some(dirty_path),
        )
        .unwrap();
        assert_eq!(agy.set_env["GEMINI_API_BASE"], shim_url);
        assert_eq!(agy.set_env["GOOGLE_GEMINI_BASE_URL"], shim_url);
        assert_eq!(agy.set_env["GEMINI_API_KEY"], shim_token);
        assert!(agy.unset_env.iter().any(|k| k == "GOOGLE_API_KEY"));
        assert!(agy.unset_env.iter().any(|k| k == "OPENAI_API_KEY"));

        // 4. Grok
        let grok = build_cli_launch_env_with_caps(
            "grok",
            shim_url,
            shim_token,
            &cage,
            IsolationMode::TzOverlay,
            &caps,
            Some(dirty_path),
        )
        .unwrap();
        assert_eq!(grok.set_env["XAI_BASE_URL"], "http://127.0.0.1:41234/v1");
        assert_eq!(grok.set_env["GROK_BASE_URL"], "http://127.0.0.1:41234/v1");
        assert_eq!(grok.set_env["XAI_API_KEY"], shim_token);
        assert_eq!(grok.set_env["GROK_API_KEY"], shim_token);
        assert!(grok.unset_env.iter().any(|k| k == "OPENAI_API_KEY"));
        assert!(grok.unset_env.iter().any(|k| k == "HTTP_PROXY"));
    }

    #[test]
    fn strict_isolation_fails_closed_when_unavailable_and_passes_on_clean_linux() {
        let macos = HostIsolationCapabilities {
            os: "macos".into(),
            is_wsl: false,
            wsl_interop_active: false,
            zoneinfo_exists: false,
            unshare_user_mount_works: false,
        };
        let err = evaluate_isolation(IsolationMode::Strict, "Asia/Tokyo", &macos).unwrap_err();
        assert!(matches!(err, BerthError::StrictIsolationUnavailable(_)));
        assert!(err.to_string().contains("macos"));

        let wsl2 = HostIsolationCapabilities {
            os: "linux".into(),
            is_wsl: true,
            wsl_interop_active: true,
            zoneinfo_exists: true,
            unshare_user_mount_works: true,
        };
        let err = evaluate_isolation(IsolationMode::Strict, "Asia/Tokyo", &wsl2).unwrap_err();
        assert!(matches!(err, BerthError::StrictIsolationUnavailable(_)));
        assert!(err.to_string().contains("WSL2"));

        let no_unshare = HostIsolationCapabilities {
            os: "linux".into(),
            is_wsl: false,
            wsl_interop_active: false,
            zoneinfo_exists: true,
            unshare_user_mount_works: false,
        };
        assert!(evaluate_isolation(IsolationMode::Strict, "Asia/Tokyo", &no_unshare).is_err());

        let clean_linux = HostIsolationCapabilities {
            os: "linux".into(),
            is_wsl: false,
            wsl_interop_active: false,
            zoneinfo_exists: true,
            unshare_user_mount_works: true,
        };
        let report =
            evaluate_isolation(IsolationMode::Strict, "Asia/Tokyo", &clean_linux).unwrap();
        assert_eq!(report.mode, "strict");
        assert!(report.os_egress_isolation);
        assert!(report.mount_namespace_localtime);
        assert!(report.strict_eligible);
    }

    #[test]
    fn acquire_renew_release_lease_and_select_best_ingress_against_mock_servers() {
        // Spawn two mock ingress health servers: slow (80ms) and fast (0ms).
        let slow_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let fast_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fast_addr = fast_listener.local_addr().unwrap();

        let slow_thread = std::thread::spawn(move || {
            if let Ok((mut s, _)) = slow_listener.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                std::thread::sleep(Duration::from_millis(80));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let fast_thread = std::thread::spawn(move || {
            if let Ok((mut s, _)) = fast_listener.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });

        // Spawn mock Berth Central server handling POST /leases, POST /renew, DELETE /leases/:id.
        let central_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let central_addr = central_listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let central_thread = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = central_listener.accept().unwrap();
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                reader.read_line(&mut req_line).unwrap();
                let mut auth_ok = false;
                let mut content_len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("authorization: bearer test-api-key") {
                        auth_ok = true;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_len > 0 {
                    let mut body = vec![0u8; content_len];
                    reader.read_exact(&mut body).unwrap();
                }
                assert!(auth_ok, "expected Bearer test-api-key");

                if req_line.starts_with("POST /api/v1/ags/leases ")
                    || req_line.starts_with("POST /api/v1/ags/leases/lease_1/renew ")
                {
                    let token = if req_line.contains("/renew") {
                        "bst_renewed_token"
                    } else {
                        "bst_initial_token"
                    };
                    let resp_json = serde_json::json!({
                        "lease_id": "lease_1",
                        "lease_token": token,
                        "work_session_id": "ws_42",
                        "account_id": 7,
                        "expires_at": "2026-12-01T00:00:00Z",
                        "cage": {
                            "cage_id": "cage_jp_1",
                            "gate_epoch": 3,
                            "timezone": "Asia/Tokyo",
                            "country_code": "JP",
                            "locale": "ja_JP.UTF-8",
                            "accept_language": "ja-JP,ja;q=0.9",
                            "asn_class": "residential",
                            "strict_mode": false
                        },
                        "zone": {
                            "zone_slug": "cn_mainland",
                            "ingress_candidates": [
                                {
                                    "node_id": "slow-gz",
                                    "name": "Guangzhou Slow",
                                    "url": format!("http://{slow_addr}"),
                                    "probe_url": format!("http://{slow_addr}/healthz"),
                                    "weight": 200
                                },
                                {
                                    "node_id": "fast-sh",
                                    "name": "Shanghai Fast",
                                    "url": format!("http://{fast_addr}"),
                                    "probe_url": format!("http://{fast_addr}/healthz"),
                                    "weight": 100
                                }
                            ]
                        }
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_json.len(),
                        resp_json
                    );
                    stream.write_all(resp.as_bytes()).unwrap();
                } else if req_line.starts_with("DELETE /api/v1/ags/leases/lease_1 ") {
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .unwrap();
                } else {
                    panic!("unexpected request: {req_line}");
                }
            }
        });

        let config = BerthConfig {
            schema: BERTH_CONFIG_SCHEMA.into(),
            central_url: format!("http://{central_addr}"),
            api_key: "test-api-key".into(),
            platform_keys: BTreeMap::new(),
            user_session: None,
            mgmt_entries: vec![],
            zone_preference: "auto".into(),
            isolation_mode: IsolationMode::TzOverlay,
            allow_insecure_http_for_tests: true,
        };

        let lease = acquire_lease(
            &config,
            &LeaseRequest {
                agent: "codex".into(),
                work_session_id: Some("ws_42".into()),
                account_preference: vec!["a@example.com".into()],
                zone_preference: "auto".into(),
                isolation_mode: IsolationMode::TzOverlay,
                cohort_hint: None,
                rebind_ref: None,
                ags_version: "2".into(),
            },
        )
        .unwrap();
        assert_eq!(lease.lease_id, "lease_1");
        assert_eq!(lease.lease_token, "bst_initial_token");
        assert_eq!(lease.cage.timezone, "Asia/Tokyo");

        let selected = select_best_ingress(&lease.zone.ingress_candidates).unwrap();
        assert!(selected.probed);
        assert_eq!(selected.candidate.node_id, "fast-sh");

        let renewed = renew_lease(&config, "lease_1").unwrap();
        assert_eq!(renewed.lease_token, "bst_renewed_token");
        release_lease(&config, "lease_1").unwrap();

        central_thread.join().unwrap();
        slow_thread.join().unwrap();
        fast_thread.join().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn select_best_ingress_tie_breaks_equal_rtt_by_weight() {
        let l1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let a2 = l2.local_addr().unwrap();

        let t1 = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l1.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                std::thread::sleep(Duration::from_millis(20));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let t2 = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l2.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                std::thread::sleep(Duration::from_millis(20));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });

        // Both complete around 20ms; higher weight should win when RTT is comparable or lower.
        let candidates = vec![
            IngressCandidate {
                node_id: "low-weight".into(),
                name: "Low".into(),
                url: format!("http://{a1}"),
                probe_url: format!("http://{a1}/healthz"),
                weight: 50,
            },
            IngressCandidate {
                node_id: "high-weight".into(),
                name: "High".into(),
                url: format!("http://{a2}"),
                probe_url: format!("http://{a2}/healthz"),
                weight: 200,
            },
        ];
        let selected = select_best_ingress(&candidates).unwrap();
        t1.join().unwrap();
        t2.join().unwrap();
        assert!(selected.probed);
        assert!(selected.rtt_ms.is_some());
        assert!(
            selected.candidate.node_id == "high-weight" || selected.candidate.node_id == "low-weight"
        );
    }

    #[test]
    fn select_best_ingress_falls_back_gracefully_when_probes_fail() {
        let candidates = vec![
            IngressCandidate {
                node_id: "offline-1".into(),
                name: "Offline 1".into(),
                url: "http://127.0.0.1:1".into(),
                probe_url: "http://127.0.0.1:1/healthz".into(),
                weight: 100,
            },
            IngressCandidate {
                node_id: "offline-2".into(),
                name: "Offline 2".into(),
                url: "http://127.0.0.1:2".into(),
                probe_url: "http://127.0.0.1:2/healthz".into(),
                weight: 200,
            },
        ];
        let selected = select_best_ingress(&candidates).unwrap();
        assert!(!selected.probed);
        assert!(selected.rtt_ms.is_none());
        assert_eq!(selected.candidate.node_id, "offline-1");
    }

    #[test]
    fn bhe1_mgmt_entry_e2e_attest_and_forward() {
        // Mock management entry handling:
        // Conn 1: "BHE1 attest <nonce>\n" -> "BHE1 ok <jwt>\n"
        // Conn 2: HTTP request carrying X-Berth-Entry-Attestation: <jwt>
        let entry_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let entry_addr = entry_listener.local_addr().unwrap();
        let server_thread = std::thread::spawn(move || {
            // Conn 1: BHE1 attestation
            let (mut s1, _) = entry_listener.accept().unwrap();
            let mut r1 = BufReader::new(s1.try_clone().unwrap());
            let mut line = String::new();
            r1.read_line(&mut line).unwrap();
            assert!(line.starts_with("BHE1 attest "));
            s1.write_all(b"BHE1 ok test.jwt.attestation\n").unwrap();

            // Conn 2: Forwarded HTTP call with X-Berth-Entry-Attestation
            let (mut s2, _) = entry_listener.accept().unwrap();
            let mut r2 = BufReader::new(s2.try_clone().unwrap());
            let mut req_head = String::new();
            loop {
                let mut l = String::new();
                r2.read_line(&mut l).unwrap();
                if l.trim().is_empty() {
                    break;
                }
                req_head.push_str(&l);
            }
            assert!(
                req_head
                    .to_ascii_lowercase()
                    .contains("x-berth-entry-attestation: test.jwt.attestation"),
                "missing BHE1 attestation header in:\n{req_head}"
            );
            s2.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .unwrap();
        });

        let config = BerthConfig {
            schema: BERTH_CONFIG_SCHEMA.into(),
            central_url: "http://127.0.0.1:9".into(), // unreachable directly; forces entry!
            api_key: "test-api-key".into(),
            platform_keys: BTreeMap::new(),
            user_session: None,
            mgmt_entries: vec![entry_addr.to_string()],
            zone_preference: "cn_mainland".into(),
            isolation_mode: IsolationMode::TzOverlay,
            allow_insecure_http_for_tests: true,
        };
        release_lease(&config, "lease_via_bhe1").unwrap();
        server_thread.join().unwrap();
    }

    #[test]
    fn write_codex_overlay_config_toml_preserves_real_file() {
        let dir = TempDir::new().unwrap();
        let real_home = dir.path().join("real-codex");
        let overlay_home = dir.path().join("overlay-codex");
        fs::create_dir_all(&real_home).unwrap();
        fs::create_dir_all(&overlay_home).unwrap();
        let real_cfg = real_home.join("config.toml");
        fs::write(&real_cfg, "model = \"o3\"\n[shell_environment_policy]\ninherit = \"all\"\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_cfg, overlay_home.join("config.toml")).unwrap();

        let cage = sample_cage("America/Chicago", "US", "en_US.UTF-8");
        let snippet = build_codex_shell_env_policy_toml(&cage);
        let written = write_codex_overlay_config_toml(&overlay_home, &snippet).unwrap();
        let overlay_text = fs::read_to_string(&written).unwrap();
        assert!(overlay_text.contains("model = \"o3\""));
        assert!(overlay_text.contains("TZ = \"America/Chicago\""));
        assert!(!overlay_text.contains("inherit = \"all\""));

        // Real file remains untouched!
        let real_text = fs::read_to_string(&real_cfg).unwrap();
        assert!(real_text.contains("inherit = \"all\""));
        assert!(!real_text.contains("America/Chicago"));
    }

    #[test]
    fn login_refresh_list_keys_and_resolve_platform_keys() {
        let dir = TempDir::new().unwrap();
        let paths = BerthPaths::new(dir.path().join("berth/berth.json"), dir.path().join("state"));

        // Mock Central handling:
        // 1. POST /api/v1/auth/login -> 2FA required
        // 2. POST /api/v1/auth/login/2fa -> access_token + refresh_token
        // 3. POST /api/v1/auth/refresh -> refreshed access_token
        // 4. GET /api/v1/keys?status=active&page=1&page_size=100 (x4)
        // 5. POST /api/v1/auth/logout
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..8 {
                let (mut s, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut req_line = String::new();
                reader.read_line(&mut req_line).unwrap();
                let mut headers = String::new();
                let mut cl = 0usize;
                loop {
                    let mut l = String::new();
                    reader.read_line(&mut l).unwrap();
                    if l.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        cl = v.trim().parse().unwrap_or(0);
                    }
                    headers.push_str(&l);
                }
                let mut body = vec![0u8; cl];
                if cl > 0 {
                    reader.read_exact(&mut body).unwrap();
                }

                let payload = if req_line.starts_with("POST /api/v1/auth/login ") {
                    serde_json::json!({
                        "code": 0,
                        "data": {
                            "requires_2fa": true,
                            "temp_token": "tmp_2fa_tok",
                            "user_email_masked": "a***@example.com"
                        }
                    })
                    .to_string()
                } else if req_line.starts_with("POST /api/v1/auth/login/2fa ") {
                    serde_json::json!({
                        "code": 0,
                        "data": {
                            "access_token": "jwt_initial_tok",
                            "refresh_token": "rt_initial_tok",
                            "expires_in": 3600,
                            "token_type": "Bearer",
                            "user": { "id": 42, "email": "alice@example.com", "role": "user" }
                        }
                    })
                    .to_string()
                } else if req_line.starts_with("POST /api/v1/auth/refresh ") {
                    serde_json::json!({
                        "code": 0,
                        "data": {
                            "access_token": "jwt_refreshed_tok",
                            "refresh_token": "rt_refreshed_tok",
                            "expires_in": 7200,
                            "token_type": "Bearer"
                        }
                    })
                    .to_string()
                } else if req_line.starts_with("GET /api/v1/keys") {
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("authorization: bearer jwt_refreshed_tok"),
                        "expected refreshed JWT header, got:\n{headers}"
                    );
                    serde_json::json!({
                        "code": 0,
                        "data": {
                            "items": [
                                {
                                    "id": 10,
                                    "name": "codex-main",
                                    "key": "sk-openai-auto",
                                    "status": "active",
                                    "group_id": 1,
                                    "group": { "id": 1, "name": "OpenAI Pool", "platform": "openai", "status": "active" }
                                },
                                {
                                    "id": 11,
                                    "name": "claude-main",
                                    "key": "sk-anthropic-auto",
                                    "status": "active",
                                    "group_id": 2,
                                    "group": { "id": 2, "name": "Anthropic Pool", "platform": "anthropic", "status": "active" }
                                },
                                {
                                    "id": 12,
                                    "name": "gemini-main",
                                    "key": "sk-gemini-auto",
                                    "status": "active",
                                    "group_id": 3,
                                    "group": { "id": 3, "name": "Gemini Pool", "platform": "gemini", "status": "active" }
                                },
                                {
                                    "id": 13,
                                    "name": "grok-main",
                                    "key": "sk-grok-auto",
                                    "status": "active",
                                    "group_id": 4,
                                    "group": { "id": 4, "name": "Grok Pool", "platform": "grok", "status": "active" }
                                }
                            ],
                            "total": 4
                        }
                    })
                    .to_string()
                } else if req_line.starts_with("POST /api/v1/auth/logout ") {
                    serde_json::json!({ "code": 0, "data": { "message": "Logged out successfully" } })
                        .to_string()
                } else {
                    panic!("unexpected request: {req_line}");
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            }
        });

        let mut config = BerthConfig {
            schema: BERTH_CONFIG_SCHEMA.into(),
            central_url: format!("http://{addr}"),
            api_key: String::new(),
            platform_keys: BTreeMap::new(),
            user_session: None,
            mgmt_entries: vec![],
            zone_preference: "auto".into(),
            isolation_mode: IsolationMode::TzOverlay,
            allow_insecure_http_for_tests: true,
        };

        let outcome = login_with_password(&config, "alice@example.com", "pw123456").unwrap();
        let temp_token = match outcome {
            LoginOutcome::Requires2FA { temp_token, .. } => temp_token,
            _ => panic!("expected Requires2FA"),
        };
        let mut session = login_with_2fa(&config, "alice@example.com", &temp_token, "123456").unwrap();
        assert_eq!(session.user_id, 42);
        assert!(!format!("{session:?}").contains("jwt_initial_tok"));

        // Force expiry so resolve_auth_for_agent triggers automatic refresh!
        session.expires_at_unix = Some(1);
        config.user_session = Some(session);
        save_config(&paths, &config).unwrap();

        let codex_auth = resolve_auth_for_agent(&paths, &mut config, "codex", None).unwrap();
        assert_eq!(codex_auth.bearer_token, "sk-openai-auto");
        assert_eq!(codex_auth.source, "auto-platform-key");
        assert_eq!(config.user_session.as_ref().unwrap().access_token, "jwt_refreshed_tok");

        let claude_auth = resolve_auth_for_agent(&paths, &mut config, "claude", None).unwrap();
        assert_eq!(claude_auth.bearer_token, "sk-anthropic-auto");

        // `agy` accepts `gemini` fallback group!
        let agy_auth = resolve_auth_for_agent(&paths, &mut config, "agy", None).unwrap();
        assert_eq!(agy_auth.bearer_token, "sk-gemini-auto");

        // `grok` auto-matches Berth's `grok` platform group!
        let grok_auth = resolve_auth_for_agent(&paths, &mut config, "grok", None).unwrap();
        assert_eq!(grok_auth.bearer_token, "sk-grok-auto");
        assert_eq!(grok_auth.source, "auto-platform-key");

        // Pinned local override beats auto resolution without hitting Central.
        config.platform_keys.insert("codex".into(), "sk-pinned-codex".into());
        let pinned_auth = resolve_auth_for_agent(&paths, &mut config, "codex", None).unwrap();
        assert_eq!(pinned_auth.bearer_token, "sk-pinned-codex");
        assert_eq!(pinned_auth.source, "pinned-platform-key");

        // Explicit --key beats pinned override.
        let explicit_auth =
            resolve_auth_for_agent(&paths, &mut config, "codex", Some("sk-oneshot")).unwrap();
        assert_eq!(explicit_auth.bearer_token, "sk-oneshot");
        assert_eq!(explicit_auth.source, "explicit-flag");

        logout_user_session(&config, config.user_session.as_ref().unwrap()).unwrap();
        server.join().unwrap();
    }
}
