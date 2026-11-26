//! Named Codex/Claude runtimes: distinct homes and an AGS-owned HTTP proxy.
//!
//! Traffic policy is one of: credential-pool issued proxy, a local HTTP
//! CONNECT listener (Clash and similar on loopback), or direct.  Ambient
//! `HTTP(S)_PROXY` / `ALL_PROXY` / `NO_PROXY` never reach the agent.  The
//! timezone the agent sees is the IANA zone of the current egress IP.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::launch::LaunchSpec;

pub const SCHEMA: &str = "ags-agent-runtime-v1";
const SELECTION_SCHEMA: &str = "ags-runtime-selection-v1";
const APPLY_HEADER: &str = "ags-agent-runtime-apply-v1";

const CREDENTIAL_UNSET: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_HOME",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_API_KEY",
];

const PROXY_UNSET: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "FTP_PROXY",
    "ftp_proxy",
];

/// Where the runtime catalog and the current selection live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub catalog: PathBuf,
    pub selection: PathBuf,
}

impl RuntimePaths {
    pub fn from_env() -> Self {
        let config = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("ags")
        } else {
            home_dir().join(".config/ags")
        };
        let state = if let Ok(dir) = std::env::var("AGENT_SESSION_STATE_DIR") {
            PathBuf::from(dir)
        } else if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
            PathBuf::from(xdg).join("ags")
        } else {
            home_dir().join(".local/state/ags")
        };
        Self {
            catalog: config.join("runtimes.json"),
            selection: state.join("runtime-selection.json"),
        }
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// On-disk catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCatalog {
    pub schema: String,
    #[serde(default)]
    pub runtimes: BTreeMap<String, AgentRuntime>,
}

impl RuntimeCatalog {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA.to_string(),
            runtimes: BTreeMap::new(),
        }
    }
}

/// Where the agent HTTP CONNECT proxy comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Pool-issued if Codex has a pool proxy, otherwise direct.
    #[default]
    Auto,
    /// User-supplied local listener (`127.0.0.1` / `localhost` / `::1`).
    Local,
    /// Always the credential-pool issued proxy when one exists.
    Pool,
    /// No proxy. Ambient terminal/global proxies are still stripped.
    Direct,
}

/// One named Codex or Claude login tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntime {
    pub provider: String,
    pub home: PathBuf,
    #[serde(default)]
    pub proxy_mode: ProxyMode,
    /// Local HTTP CONNECT URL when `proxy_mode` is `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Legacy catalog fields. Read, then dropped on the next save.
    #[serde(default, skip_serializing)]
    pub login_proxy: Option<String>,
    #[serde(default, skip_serializing)]
    pub egress_proxy: Option<String>,
    #[serde(default, skip_serializing)]
    pub login_timezone: Option<String>,
    #[serde(default, skip_serializing)]
    pub egress_timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuntimeSelection {
    schema: String,
    name: String,
}

/// What a launch or bind step should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeIntent {
    /// Rebind `$codex_home` / `$claude_home` only (list/save/restore/pool).
    Homes,
    /// Homes plus the login/OAuth/pool HTTP proxy.
    Login,
    /// Homes plus the agent-child HTTP proxy.
    Egress,
    /// Login HTTP proxy without rebinding homes (after an overlay).
    LoginProxy,
    /// Egress HTTP proxy without rebinding homes (after an overlay).
    EgressProxy,
}

impl RuntimeIntent {
    pub fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value.trim() {
            "homes" => Ok(Self::Homes),
            "login" => Ok(Self::Login),
            "egress" | "launch" => Ok(Self::Egress),
            "login-proxy" => Ok(Self::LoginProxy),
            "egress-proxy" => Ok(Self::EgressProxy),
            other => Err(RuntimeError::Invalid(format!(
                "runtime intent must be homes, login, egress, login-proxy, or egress-proxy; got {other}"
            ))),
        }
    }

    fn includes_homes(self) -> bool {
        matches!(self, Self::Homes | Self::Login | Self::Egress)
    }

    fn includes_proxy(self) -> bool {
        !matches!(self, Self::Homes)
    }
}

/// Resolved runtime plus the name it was selected under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRuntime {
    pub name: String,
    pub runtime: AgentRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    Invalid(String),
    NotFound(String),
    Io(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::NotFound(message) | Self::Io(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<io::Error> for RuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// Validate a runtime id (`ags --runtime NAME` / catalog key).
pub fn validate_runtime_name(name: &str) -> Result<(), RuntimeError> {
    if name.is_empty() || name.len() > 64 {
        return Err(RuntimeError::Invalid(
            "runtime name must be 1-64 letters, numbers, dots, underscores, or hyphens".into(),
        ));
    }
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return Err(RuntimeError::Invalid("runtime name is empty".into()));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(RuntimeError::Invalid(
            "runtime name must start with a letter or number".into(),
        ));
    }
    if !characters.all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
    }) {
        return Err(RuntimeError::Invalid(
            "runtime name must be letters, numbers, dots, underscores, or hyphens".into(),
        ));
    }
    Ok(())
}

pub fn validate_provider(provider: &str) -> Result<(), RuntimeError> {
    match provider {
        "codex" | "claude" => Ok(()),
        other => Err(RuntimeError::Invalid(format!(
            "runtime provider must be codex or claude; got {other}"
        ))),
    }
}

/// HTTP CONNECT only. SOCKS, TLS-to-proxy, and userinfo are refused.
pub fn validate_proxy_url(raw: &str) -> Result<(), RuntimeError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(RuntimeError::Invalid("proxy URL is empty".into()));
    }
    let scheme = url.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some("http") => {}
        Some("https") => {
            return Err(RuntimeError::Invalid(
                "HTTPS proxy URLs are not supported; use http://host:port (HTTP CONNECT)".into(),
            ));
        }
        Some(other) if other.eq_ignore_ascii_case("socks")
            || other.eq_ignore_ascii_case("socks5")
            || other.eq_ignore_ascii_case("socks4")
            || other.eq_ignore_ascii_case("socks5h") =>
        {
            return Err(RuntimeError::Invalid(
                "SOCKS proxies are not supported for Codex or Claude service traffic".into(),
            ));
        }
        Some(_) | None => {
            return Err(RuntimeError::Invalid(
                "proxy URL must be http://host:port with no credentials".into(),
            ));
        }
    }
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    if rest.contains('@') {
        return Err(RuntimeError::Invalid(
            "proxy URLs must not contain credentials; put auth on a local unauthenticated listener"
                .into(),
        ));
    }
    if rest.is_empty() || rest.starts_with('/') {
        return Err(RuntimeError::Invalid("proxy URL is missing a host".into()));
    }
    Ok(())
}

fn proxy_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    if let Some(rest) = rest.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(&rest[..end]);
    }
    Some(rest.split([':', '/']).next().unwrap_or(rest))
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("localhost.localdomain")
    {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|addr| addr.is_loopback())
}

/// Custom proxies are local listeners only (Clash, mihomo, v2ray on loopback).
pub fn validate_local_proxy_url(raw: &str) -> Result<(), RuntimeError> {
    validate_proxy_url(raw)?;
    let host = proxy_host(raw).unwrap_or("");
    if !is_loopback_host(host) {
        return Err(RuntimeError::Invalid(
            "custom proxy must be a local listener (http://127.0.0.1:port or http://localhost:port)"
                .into(),
        ));
    }
    Ok(())
}

/// `--proxy URL|pool|none|auto`
pub fn parse_proxy_flag(value: &str) -> Result<(ProxyMode, Option<String>), RuntimeError> {
    match value.trim() {
        "" | "auto" => Ok((ProxyMode::Auto, None)),
        "none" | "direct" | "off" => Ok((ProxyMode::Direct, None)),
        "pool" => Ok((ProxyMode::Pool, None)),
        url => {
            validate_local_proxy_url(url)?;
            Ok((ProxyMode::Local, Some(url.to_string())))
        }
    }
}

pub fn pool_issued_proxy_from_json(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let url = value
        .pointer("/data/proxy")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/data/egress_proxy")
                .and_then(serde_json::Value::as_str)
        })?;
    validate_proxy_url(url).ok()?;
    Some(url.to_string())
}

/// IANA name only. Rejects offsets-as-names except UTC/GMT and path tricks.
pub fn validate_timezone(name: &str) -> Result<(), RuntimeError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(RuntimeError::Invalid(
            "timezone must be an IANA name such as America/Chicago".into(),
        ));
    }
    if name.starts_with('/') || name.contains("..") || name.contains('\\') {
        return Err(RuntimeError::Invalid("timezone must be an IANA name".into()));
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic());
    let chars_ok = name.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '+' | '-')
    });
    if !first_ok || !chars_ok {
        return Err(RuntimeError::Invalid(format!(
            "timezone must be an IANA name such as America/Chicago; got {name}"
        )));
    }
    if matches!(name, "UTC" | "GMT" | "Etc/UTC" | "Etc/GMT" | "Z") {
        return Ok(());
    }
    if !name.contains('/') {
        return Err(RuntimeError::Invalid(format!(
            "timezone must be an IANA name such as America/Chicago; got {name}"
        )));
    }
    for root in ["/usr/share/zoneinfo", "/var/db/timezone/zoneinfo"] {
        if Path::new(root).join(name).is_file() {
            return Ok(());
        }
    }
    if !Path::new("/usr/share/zoneinfo").exists()
        && !Path::new("/var/db/timezone/zoneinfo").exists()
    {
        return Ok(());
    }
    Err(RuntimeError::Invalid(format!("unknown IANA timezone {name}")))
}

/// Region facts derived from the current egress IP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRegion {
    pub timezone: String,
    pub country_code: Option<String>,
}

/// glibc locale for an ISO 3166-1 alpha-2 country code.
pub fn locale_for_country(code: &str) -> String {
    let lang = match code.to_ascii_uppercase().as_str() {
        "JP" => "ja_JP",
        "CN" => "zh_CN",
        "HK" => "zh_HK",
        "MO" => "zh_MO",
        "TW" => "zh_TW",
        "KR" => "ko_KR",
        "DE" => "de_DE",
        "FR" => "fr_FR",
        "GB" => "en_GB",
        "AU" => "en_AU",
        "SG" => "en_SG",
        "US" | "CA" => "en_US",
        "BR" => "pt_BR",
        "ES" => "es_ES",
        "IT" => "it_IT",
        "NL" => "nl_NL",
        "IN" => "en_IN",
        _ => "en_US",
    };
    format!("{lang}.UTF-8")
}

/// Read timezone and optional country from a free ipwho.is JSON body.
pub fn region_from_ipwho_json(raw: &str) -> Result<EgressRegion, RuntimeError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
        RuntimeError::Invalid(format!("ipwho.is response is not JSON: {error}"))
    })?;
    if value.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(RuntimeError::Invalid("ipwho.is lookup failed".into()));
    }
    let timezone = value.get("timezone").ok_or_else(|| {
        RuntimeError::Invalid("ipwho.is response has no timezone".into())
    })?;
    let name = timezone
        .as_str()
        .or_else(|| timezone.get("id").and_then(serde_json::Value::as_str))
        .or_else(|| timezone.get("time_zone").and_then(serde_json::Value::as_str))
        .ok_or_else(|| RuntimeError::Invalid("ipwho.is timezone is missing an IANA id".into()))?;
    validate_timezone(name)?;
    let country_code = value
        .get("country_code")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|code| {
            code.len() == 2 && code.chars().all(|character| character.is_ascii_alphabetic())
        })
        .map(|code| code.to_ascii_uppercase());
    Ok(EgressRegion {
        timezone: name.to_string(),
        country_code,
    })
}

/// Read an IANA id out of a free ipwho.is (or ipwho.org-shaped) JSON body.
pub fn timezone_from_ipwho_json(raw: &str) -> Result<String, RuntimeError> {
    Ok(region_from_ipwho_json(raw)?.timezone)
}

/// Look up timezone and country of the current egress IP (through `proxy` if set).
pub fn probe_region(proxy: Option<&str>) -> Result<EgressRegion, RuntimeError> {
    if std::env::var("AGENT_SESSION_DISABLE_GEO")
        .ok()
        .as_deref()
        == Some("1")
    {
        return Err(RuntimeError::Invalid("geo lookup is disabled".into()));
    }
    let mut command = Command::new("curl");
    command.args([
        "-fsS",
        "--connect-timeout",
        "2",
        "--max-time",
        "5",
        "--max-filesize",
        "65536",
    ]);
    if let Some(proxy) = proxy {
        command.args(["-x", proxy]);
    }
    let output = command
        .arg("https://ipwho.is/?fields=success,ip,timezone,country_code")
        .output()
        .map_err(|error| RuntimeError::Io(format!("could not probe egress timezone: {error}")))?;
    if !output.status.success() {
        return Err(RuntimeError::Invalid(
            "could not derive timezone from the egress IP".into(),
        ));
    }
    region_from_ipwho_json(&String::from_utf8_lossy(&output.stdout))
}

/// Look up the IANA timezone of the current egress IP (through `proxy` if set).
pub fn probe_timezone(proxy: Option<&str>) -> Result<String, RuntimeError> {
    Ok(probe_region(proxy)?.timezone)
}

pub fn canonicalize_runtime(runtime: &mut AgentRuntime) -> Result<(), RuntimeError> {
    if runtime.proxy.is_none() {
        if let Some(url) = runtime
            .egress_proxy
            .take()
            .or_else(|| runtime.login_proxy.take())
        {
            runtime.proxy = Some(url);
            if runtime.proxy_mode == ProxyMode::Auto {
                runtime.proxy_mode = ProxyMode::Local;
            }
        }
    }
    runtime.login_proxy = None;
    runtime.egress_proxy = None;
    runtime.login_timezone = None;
    runtime.egress_timezone = None;
    match runtime.proxy_mode {
        ProxyMode::Local => {
            let url = runtime.proxy.as_deref().ok_or_else(|| {
                RuntimeError::Invalid("proxy mode local requires --proxy http://127.0.0.1:port".into())
            })?;
            validate_local_proxy_url(url)?;
        }
        ProxyMode::Auto | ProxyMode::Pool | ProxyMode::Direct => {
            runtime.proxy = None;
        }
    }
    Ok(())
}

pub fn validate_home(path: &Path) -> Result<(), RuntimeError> {
    if !path.is_absolute() {
        return Err(RuntimeError::Invalid(format!(
            "runtime home must be an absolute path: {}",
            path.display()
        )));
    }
    let rendered = path.to_string_lossy();
    if rendered.contains("/OneDrive/")
        || rendered.contains("/CloudStorage/")
        || rendered.ends_with("/OneDrive")
        || rendered.ends_with("/CloudStorage")
    {
        return Err(RuntimeError::Invalid(
            "runtime home cannot live on OneDrive or CloudStorage".into(),
        ));
    }
    let metadata = fs::metadata(path).map_err(|_| {
        RuntimeError::Invalid(format!(
            "runtime home must already exist as a directory: {}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(RuntimeError::Invalid(format!(
            "runtime home is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn validate_runtime(name: &str, runtime: &AgentRuntime) -> Result<(), RuntimeError> {
    validate_runtime_name(name)?;
    validate_provider(&runtime.provider)?;
    validate_home(&runtime.home)?;
    match runtime.proxy_mode {
        ProxyMode::Local => {
            let url = runtime.proxy.as_deref().ok_or_else(|| {
                RuntimeError::Invalid("proxy mode local requires a loopback HTTP proxy URL".into())
            })?;
            validate_local_proxy_url(url)?;
        }
        ProxyMode::Auto | ProxyMode::Pool | ProxyMode::Direct => {}
    }
    Ok(())
}

pub fn load_catalog(paths: &RuntimePaths) -> Result<RuntimeCatalog, RuntimeError> {
    if !paths.catalog.is_file() {
        return Ok(RuntimeCatalog::empty());
    }
    let raw = fs::read_to_string(&paths.catalog)?;
    let mut catalog: RuntimeCatalog = serde_json::from_str(&raw)?;
    if catalog.schema != SCHEMA {
        return Err(RuntimeError::Invalid(format!(
            "unsupported runtime catalog schema: {}",
            catalog.schema
        )));
    }
    for (name, runtime) in catalog.runtimes.iter_mut() {
        canonicalize_runtime(runtime)?;
        validate_runtime(name, runtime)?;
    }
    Ok(catalog)
}

pub fn save_catalog(paths: &RuntimePaths, catalog: &RuntimeCatalog) -> Result<(), RuntimeError> {
    if catalog.schema != SCHEMA {
        return Err(RuntimeError::Invalid(
            "runtime catalog schema must be ags-agent-runtime-v1".into(),
        ));
    }
    for (name, runtime) in &catalog.runtimes {
        validate_runtime(name, runtime)?;
    }
    atomic_write(
        &paths.catalog,
        serde_json::to_string_pretty(catalog)?.as_bytes(),
    )
}

pub fn load_selection(paths: &RuntimePaths) -> Result<Option<String>, RuntimeError> {
    if !paths.selection.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&paths.selection)?;
    let row: RuntimeSelection = serde_json::from_str(&raw)?;
    if row.schema != SELECTION_SCHEMA {
        return Err(RuntimeError::Invalid(format!(
            "unsupported runtime selection schema: {}",
            row.schema
        )));
    }
    validate_runtime_name(&row.name)?;
    Ok(Some(row.name))
}

pub fn save_selection(paths: &RuntimePaths, name: &str) -> Result<(), RuntimeError> {
    validate_runtime_name(name)?;
    let catalog = load_catalog(paths)?;
    if !catalog.runtimes.contains_key(name) {
        return Err(RuntimeError::NotFound(format!(
            "unknown runtime {name}; add it with ags runtime add"
        )));
    }
    let row = RuntimeSelection {
        schema: SELECTION_SCHEMA.to_string(),
        name: name.to_string(),
    };
    atomic_write(
        &paths.selection,
        serde_json::to_string_pretty(&row)?.as_bytes(),
    )
}

pub fn clear_selection(paths: &RuntimePaths) -> Result<(), RuntimeError> {
    match fs::remove_file(&paths.selection) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `--runtime NAME` wins over the persisted selection. Empty means none.
pub fn resolve(
    paths: &RuntimePaths,
    explicit: Option<&str>,
) -> Result<Option<ResolvedRuntime>, RuntimeError> {
    let name = if let Some(explicit) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        validate_runtime_name(explicit)?;
        Some(explicit.to_string())
    } else {
        load_selection(paths)?
    };
    let Some(name) = name else {
        return Ok(None);
    };
    let catalog = load_catalog(paths)?;
    let runtime = catalog.runtimes.get(&name).cloned().ok_or_else(|| {
        RuntimeError::NotFound(format!(
            "unknown runtime {name}; add it with ags runtime add"
        ))
    })?;
    validate_runtime(&name, &runtime)?;
    Ok(Some(ResolvedRuntime { name, runtime }))
}

/// Refuse `ags --runtime work-codex claude` and a persisted Codex runtime on `ags claude`.
pub fn provider_matches_command(
    resolved: &ResolvedRuntime,
    command: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(command) = command else {
        return Ok(());
    };
    match command {
        "codex" | "claude" if command != resolved.runtime.provider => {
            Err(RuntimeError::Invalid(format!(
                "runtime {} is provider={} and cannot launch {command}",
                resolved.name, resolved.runtime.provider
            )))
        }
        _ => Ok(()),
    }
}

pub fn intent_for_agent_args(provider: &str, args: &[String]) -> RuntimeIntent {
    let Some(first) = args.first().map(String::as_str) else {
        return RuntimeIntent::Egress;
    };
    match (provider, first) {
        ("codex", "login") | ("claude", "login" | "setup-token") => RuntimeIntent::Login,
        _ => RuntimeIntent::Egress,
    }
}

fn selected_proxy(runtime: &AgentRuntime, intent: RuntimeIntent) -> Option<&str> {
    if !intent.includes_proxy() {
        return None;
    }
    match runtime.proxy_mode {
        ProxyMode::Local => runtime.proxy.as_deref(),
        ProxyMode::Auto | ProxyMode::Pool | ProxyMode::Direct => None,
    }
}

pub fn selected_proxy_mode(runtime: &AgentRuntime, intent: RuntimeIntent) -> ProxyMode {
    if !intent.includes_proxy() {
        return ProxyMode::Direct;
    }
    runtime.proxy_mode
}

fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Drop `/mnt/<drive>/...` PATH entries (WSL DrvFs Windows trees).
pub fn strip_windows_drvfs_path(path: &str) -> String {
    let kept: Vec<&str> = path
        .split(':')
        .filter(|part| !part.is_empty() && !is_wsl_windows_path(part))
        .collect();
    if kept.is_empty() {
        "/usr/local/bin:/usr/bin:/bin".into()
    } else {
        kept.join(":")
    }
}

fn is_wsl_windows_path(part: &str) -> bool {
    let bytes = part.as_bytes();
    bytes.len() >= 6
        && part.starts_with("/mnt/")
        && bytes[5].is_ascii_alphabetic()
        && (bytes.len() == 6 || bytes[6] == b'/')
}

/// Shell fragment the launcher evals. Empty when no runtime is selected.
pub fn render_apply(
    resolved: Option<&ResolvedRuntime>,
    intent: RuntimeIntent,
    command: Option<&str>,
) -> Result<String, RuntimeError> {
    let mut lines = vec![format!("# {APPLY_HEADER}")];
    let Some(resolved) = resolved else {
        lines.push("# none".into());
        return Ok(lines.join("\n") + "\n");
    };
    provider_matches_command(resolved, command)?;
    lines.push(format!(
        "# name={} provider={}",
        resolved.name, resolved.runtime.provider
    ));
    if intent.includes_homes() {
        let quoted_home = posix_single_quote(&resolved.runtime.home.display().to_string());
        match resolved.runtime.provider.as_str() {
            "codex" => {
                lines.push(format!("codex_home={quoted_home}"));
                lines.push(format!("export CODEX_HOME={quoted_home}"));
            }
            "claude" => {
                lines.push(format!("claude_home={quoted_home}"));
                lines.push(format!("export CLAUDE_CONFIG_DIR={quoted_home}"));
                lines.push("unset CLAUDE_HOME".into());
            }
            _ => {}
        }
        lines.push(format!(
            "export AGS_RUNTIME={}",
            posix_single_quote(&resolved.name)
        ));
        for key in CREDENTIAL_UNSET {
            if *key == "CLAUDE_HOME" && resolved.runtime.provider == "claude" {
                continue;
            }
            lines.push(format!("unset {key}"));
        }
    }
    if intent.includes_proxy() {
        for key in PROXY_UNSET {
            lines.push(format!("unset {key}"));
        }
        let mode = selected_proxy_mode(&resolved.runtime, intent);
        lines.push(format!(
            "AGS_RUNTIME_PROXY_MODE={}",
            posix_single_quote(match mode {
                ProxyMode::Auto => "auto",
                ProxyMode::Local => "local",
                ProxyMode::Pool => "pool",
                ProxyMode::Direct => "direct",
            })
        ));
        if let Some(proxy) = selected_proxy(&resolved.runtime, intent) {
            let quoted = posix_single_quote(proxy);
            lines.push(format!("export HTTPS_PROXY={quoted}"));
            lines.push(format!("export HTTP_PROXY={quoted}"));
            lines.push(format!("export https_proxy={quoted}"));
            lines.push(format!("export http_proxy={quoted}"));
        }
    }
    Ok(lines.join("\n") + "\n")
}

/// Convert-launch path: proxy and credential scrub only. Homes come from the
/// process environment the bash launcher already exported.
pub fn apply_to_spec(
    mut spec: LaunchSpec,
    resolved: Option<&ResolvedRuntime>,
    intent: RuntimeIntent,
) -> Result<LaunchSpec, RuntimeError> {
    for key in PROXY_UNSET {
        spec = spec.without_env(*key);
    }
    spec = spec.without_env("WT_SESSION").without_env("WT_PROFILE_ID");
    let inherited_path = spec
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("PATH").ok());
    if let Some(path) = inherited_path {
        spec = spec.with_env("PATH", strip_windows_drvfs_path(&path));
    }
    let Some(resolved) = resolved else {
        return Ok(spec);
    };
    for key in CREDENTIAL_UNSET {
        spec = spec.without_env(*key);
    }
    if let Some(proxy) = selected_proxy(&resolved.runtime, intent) {
        spec = spec
            .with_env("HTTPS_PROXY", proxy)
            .with_env("HTTP_PROXY", proxy)
            .with_env("https_proxy", proxy)
            .with_env("http_proxy", proxy);
    }
    spec = spec.with_env("AGS_RUNTIME", &resolved.name);
    Ok(spec)
}

pub fn add_runtime(
    paths: &RuntimePaths,
    name: &str,
    mut runtime: AgentRuntime,
) -> Result<(), RuntimeError> {
    validate_runtime_name(name)?;
    validate_provider(&runtime.provider)?;
    canonicalize_runtime(&mut runtime)?;
    if !runtime.home.exists() {
        fs::create_dir_all(&runtime.home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime.home, fs::Permissions::from_mode(0o700))?;
        }
    }
    validate_home(&runtime.home)?;
    let mut catalog = load_catalog(paths)?;
    catalog.schema = SCHEMA.to_string();
    catalog.runtimes.insert(name.to_string(), runtime);
    save_catalog(paths, &catalog)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_in(root: &Path) -> RuntimePaths {
        RuntimePaths {
            catalog: root.join("config/runtimes.json"),
            selection: root.join("state/runtime-selection.json"),
        }
    }

    fn make_home(root: &Path, name: &str) -> PathBuf {
        let home = root.join(name);
        fs::create_dir_all(&home).unwrap();
        home
    }

    fn runtime(provider: &str, home: PathBuf) -> AgentRuntime {
        AgentRuntime {
            provider: provider.into(),
            home,
            proxy_mode: ProxyMode::Auto,
            proxy: None,
            login_proxy: None,
            egress_proxy: None,
            login_timezone: None,
            egress_timezone: None,
        }
    }

    fn local_runtime(provider: &str, home: PathBuf, proxy: &str) -> AgentRuntime {
        AgentRuntime {
            provider: provider.into(),
            home,
            proxy_mode: ProxyMode::Local,
            proxy: Some(proxy.into()),
            login_proxy: None,
            egress_proxy: None,
            login_timezone: None,
            egress_timezone: None,
        }
    }

    #[test]
    fn rejects_socks_and_https_and_userinfo() {
        assert!(validate_proxy_url("socks5://127.0.0.1:7890").is_err());
        assert!(validate_proxy_url("socks://127.0.0.1:7890").is_err());
        assert!(validate_proxy_url("https://127.0.0.1:7890").is_err());
        assert!(validate_proxy_url("http://user:pass@127.0.0.1:7890").is_err());
        assert!(validate_proxy_url("http://127.0.0.1:7890").is_ok());
        assert!(validate_local_proxy_url("http://127.0.0.1:7890").is_ok());
        assert!(validate_local_proxy_url("http://localhost:7890").is_ok());
        assert!(validate_local_proxy_url("http://1.2.3.4:7890").is_err());
        assert_eq!(
            parse_proxy_flag("http://127.0.0.1:7890").unwrap(),
            (ProxyMode::Local, Some("http://127.0.0.1:7890".into()))
        );
        assert_eq!(parse_proxy_flag("pool").unwrap(), (ProxyMode::Pool, None));
        assert_eq!(parse_proxy_flag("none").unwrap(), (ProxyMode::Direct, None));
        assert_eq!(
            pool_issued_proxy_from_json(r#"{"data":{"proxy":"http://10.0.0.2:8080"}}"#)
                .as_deref(),
            Some("http://10.0.0.2:8080")
        );
        assert!(pool_issued_proxy_from_json(r#"{"data":{"proxy":"socks5://10.0.0.2:1080"}}"#).is_none());
    }

    #[test]
    fn rejects_onedrive_home() {
        let err = validate_home(Path::new(
            "/mnt/c/Users/x/OneDrive/05_MyWork/.codex-work",
        ));
        assert!(err.is_err(), "{err:?}");
    }

    #[test]
    fn add_and_resolve_round_trip() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let home = make_home(tmp.path(), "codex-work");
        add_runtime(
            &paths,
            "work-codex",
            local_runtime("codex", home.clone(), "http://127.0.0.1:7890"),
        )
        .unwrap();
        save_selection(&paths, "work-codex").unwrap();
        let resolved = resolve(&paths, None).unwrap().unwrap();
        assert_eq!(resolved.name, "work-codex");
        assert_eq!(resolved.runtime.home, home);
        assert!(provider_matches_command(&resolved, Some("claude")).is_err());
        assert!(provider_matches_command(&resolved, Some("codex")).is_ok());
        assert!(provider_matches_command(&resolved, Some("list")).is_ok());
    }

    #[test]
    fn explicit_runtime_overrides_selection() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let work = make_home(tmp.path(), "work");
        let personal = make_home(tmp.path(), "personal");
        add_runtime(&paths, "work-codex", runtime("codex", work)).unwrap();
        add_runtime(
            &paths,
            "personal-codex",
            runtime("codex", personal.clone()),
        )
        .unwrap();
        save_selection(&paths, "work-codex").unwrap();
        let resolved = resolve(&paths, Some("personal-codex")).unwrap().unwrap();
        assert_eq!(resolved.name, "personal-codex");
        assert_eq!(resolved.runtime.home, personal);
    }

    #[test]
    fn apply_shell_sets_local_proxy_for_login_and_egress() {
        let tmp = TempDir::new().unwrap();
        let home = make_home(tmp.path(), "codex-work");
        let resolved = ResolvedRuntime {
            name: "work-codex".into(),
            runtime: local_runtime("codex", home, "http://127.0.0.1:7890"),
        };
        let homes = render_apply(Some(&resolved), RuntimeIntent::Homes, Some("list")).unwrap();
        assert!(homes.contains("export CODEX_HOME="));
        assert!(!homes.contains("HTTPS_PROXY"));
        let egress = render_apply(Some(&resolved), RuntimeIntent::Egress, Some("codex")).unwrap();
        assert!(egress.contains("http://127.0.0.1:7890"));
        assert!(egress.contains("AGS_RUNTIME_PROXY_MODE='local'"));
        assert!(!egress.contains("export TZ="));
        let login = render_apply(Some(&resolved), RuntimeIntent::Login, Some("codex")).unwrap();
        assert!(login.contains("http://127.0.0.1:7890"));
        let proxy_only =
            render_apply(Some(&resolved), RuntimeIntent::EgressProxy, Some("codex")).unwrap();
        assert!(proxy_only.contains("http://127.0.0.1:7890"));
        assert!(!proxy_only.contains("export CODEX_HOME="));
        let empty = render_apply(None, RuntimeIntent::Egress, Some("codex")).unwrap();
        assert!(empty.contains("# none"));
        assert!(!empty.contains("export CODEX_HOME"));
    }

    #[test]
    fn login_intent_from_codex_login_args() {
        assert_eq!(
            intent_for_agent_args("codex", &["login".into()]),
            RuntimeIntent::Login
        );
        assert_eq!(
            intent_for_agent_args("codex", &[]),
            RuntimeIntent::Egress
        );
        assert_eq!(
            intent_for_agent_args("claude", &["setup-token".into()]),
            RuntimeIntent::Login
        );
    }

    #[test]
    fn apply_to_spec_scrubs_ambient_proxy_without_runtime() {
        let spec = LaunchSpec::new("codex", ["exec".into()]);
        let spec = apply_to_spec(spec, None, RuntimeIntent::Egress).unwrap();
        assert!(!spec.env.iter().any(|(key, _)| key == "HTTPS_PROXY"));
        assert!(spec.env_removals().iter().any(|key| key == "HTTPS_PROXY"));
        assert!(spec.env_removals().iter().any(|key| key == "ALL_PROXY"));
        assert!(spec.env_removals().iter().any(|key| key == "NO_PROXY"));
    }

    #[test]
    fn legacy_split_proxies_become_one_local_listener() {
        let tmp = TempDir::new().unwrap();
        let mut runtime = runtime("codex", make_home(tmp.path(), "home"));
        runtime.login_proxy = Some("http://127.0.0.1:7890".into());
        runtime.egress_proxy = Some("http://127.0.0.1:18080".into());
        canonicalize_runtime(&mut runtime).unwrap();
        assert_eq!(runtime.proxy_mode, ProxyMode::Local);
        assert_eq!(runtime.proxy.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(runtime.login_proxy, None);
    }

    #[test]
    fn ipwho_json_reads_iana_id() {
        let id = timezone_from_ipwho_json(
            r#"{"success":true,"ip":"1.2.3.4","timezone":{"id":"America/Chicago","offset":-21600}}"#,
        )
        .unwrap();
        assert_eq!(id, "America/Chicago");
        let org = timezone_from_ipwho_json(
            r#"{"success":true,"timezone":{"time_zone":"Asia/Tokyo"}}"#,
        )
        .unwrap();
        assert_eq!(org, "Asia/Tokyo");
        assert!(timezone_from_ipwho_json(r#"{"success":false}"#).is_err());
        assert!(validate_timezone("not a zone").is_err());
        assert!(validate_timezone("America/Chicago").is_ok());
        let region = region_from_ipwho_json(
            r#"{"success":true,"country_code":"jp","timezone":{"id":"Asia/Tokyo"}}"#,
        )
        .unwrap();
        assert_eq!(region.timezone, "Asia/Tokyo");
        assert_eq!(region.country_code.as_deref(), Some("JP"));
        assert_eq!(locale_for_country("JP"), "ja_JP.UTF-8");
        assert_eq!(locale_for_country("us"), "en_US.UTF-8");
    }

    #[test]
    fn strip_windows_drvfs_path_drops_drive_mounts() {
        let stripped = strip_windows_drvfs_path(
            "/usr/bin:/mnt/c/WINDOWS/system32:/bin:/mnt/c/WINDOWS/System32/WindowsPowerShell/v1.0/",
        );
        assert_eq!(stripped, "/usr/bin:/bin");
        assert!(!stripped.contains("/mnt/c"));
        assert_eq!(
            strip_windows_drvfs_path("/mnt/c/WINDOWS/system32"),
            "/usr/local/bin:/usr/bin:/bin"
        );
        assert!(!strip_windows_drvfs_path("/usr/bin:/mnt/cache/bin").contains("WINDOWS"));
        assert!(strip_windows_drvfs_path("/usr/bin:/mnt/cache/bin").contains("/mnt/cache/bin"));
    }

    #[test]
    fn auto_mode_emits_mode_without_proxy_url() {
        let tmp = TempDir::new().unwrap();
        let resolved = ResolvedRuntime {
            name: "work-codex".into(),
            runtime: runtime("codex", make_home(tmp.path(), "codex-work")),
        };
        let egress = render_apply(Some(&resolved), RuntimeIntent::Egress, Some("codex")).unwrap();
        assert!(egress.contains("AGS_RUNTIME_PROXY_MODE='auto'"));
        assert!(!egress.contains("export HTTPS_PROXY="));
        assert!(!egress.contains("export TZ="));
    }
}
