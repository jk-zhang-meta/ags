#![forbid(unsafe_code)]

//! Self-update and automatic background update checks for `berth` (`bh`) CLI.
//!
//! Fetches releases from GitHub Releases (`jk-zhang-meta/berth-cli` by default),
//! strictly verifies the SHA-256 digest (and Cosign bundle when `cosign` is
//! installed), smoke-tests the candidate binary, and atomically replaces the
//! running executable in place.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::berth::build_tls_client_config;

pub const DEFAULT_UPDATE_REPO: &str = "jk-zhang-meta/berth-cli";
const DEFAULT_API_BASE: &str = "https://api.github.com";
const AUTO_CHECK_TTL_SECS: u64 = 6 * 3600;
const MAX_REDIRECTS: usize = 6;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_META_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub updated: bool,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AutoUpdateState {
    last_checked_epoch: u64,
    #[serde(default)]
    latest_version: String,
    #[serde(default)]
    last_error: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
pub struct ReleaseAssets {
    pub version: String,
    pub target: String,
    pub archive_url: String,
    pub sha256_url: String,
    pub sigstore_url: Option<String>,
}

/// Detect the dist.yml release target triple for the current platform.
pub fn current_target_triple() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        (os, arch) => Err(format!("unsupported platform for self-update: {os}/{arch}")),
    }
}

/// Parse a SemVer triple `major.minor.patch` (ignoring leading `v` and any `-pre`/`+build`).
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches('v');
    let core = trimmed.split(['-', '+', ' ']).next().unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    Some((major, minor, patch))
}

/// Return true iff `candidate` is strictly newer than `current`.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

/// Parse the first 64-character hex SHA-256 token from a `.sha256` file.
pub fn parse_sha256_file(contents: &str) -> Result<String, String> {
    let token = contents
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid sha256 checksum file: {contents:?}"));
    }
    Ok(token)
}

fn update_repo() -> String {
    std::env::var("BERTH_UPDATE_REPO")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_REPO.to_string())
}

fn api_base() -> String {
    std::env::var("BERTH_UPDATE_API_BASE")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

fn github_token() -> Option<String> {
    std::env::var("BERTH_UPDATE_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BERTH_UPDATE_STATE_PATH") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/berth/update-check.json"))
}

fn read_state(path: &Path) -> AutoUpdateState {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_state(path: &Path, state: &AutoUpdateState) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(path, json);
    }
}

/// Run explicit `bh update` / `bh self-update`.
pub fn run_update(check_only: bool, force: bool) -> Result<UpdateCheckResult, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let target = current_target_triple()?.to_string();
    let repo = update_repo();
    let base = api_base();
    let assets = fetch_latest_release(&base, &repo, &target, Duration::from_secs(15))?;
    let available = is_newer_version(&current, &assets.version);

    if let Some(state_path) = state_file_path() {
        write_state(
            &state_path,
            &AutoUpdateState {
                last_checked_epoch: now_unix_secs(),
                latest_version: assets.version.clone(),
                last_error: String::new(),
            },
        );
    }

    if check_only {
        return Ok(UpdateCheckResult {
            current_version: current,
            latest_version: assets.version,
            update_available: available,
            updated: false,
            target,
            sha256: None,
            executable: None,
            message: Some(if available {
                "A newer berth release is available; run `bh update` to install.".to_string()
            } else {
                "berth is already up to date.".to_string()
            }),
        });
    }

    if !available && !force {
        return Ok(UpdateCheckResult {
            current_version: current,
            latest_version: assets.version,
            update_available: false,
            updated: false,
            target,
            sha256: None,
            executable: None,
            message: Some("berth is already up to date.".to_string()),
        });
    }

    let exe_path = resolve_current_exe()?;
    let sha = download_verify_and_install(&assets, &repo, &exe_path, Duration::from_secs(60))?;
    Ok(UpdateCheckResult {
        current_version: current,
        latest_version: assets.version,
        update_available: available,
        updated: true,
        target,
        sha256: Some(sha),
        executable: Some(exe_path.display().to_string()),
        message: Some("berth updated successfully.".to_string()),
    })
}

/// Low-latency automatic update check invoked on interactive CLI runs.
///
/// Uses a 6-hour state cache so regular invocations incur zero network latency.
/// Fails open on offline/network errors and fails closed on SHA-256 verification.
pub fn maybe_auto_update_on_startup(json_mode: bool) {
    if json_mode || auto_update_disabled() {
        return;
    }
    let Some(state_path) = state_file_path() else {
        return;
    };
    let now = now_unix_secs();
    let mut state = read_state(&state_path);
    if state.last_checked_epoch > 0
        && now.saturating_sub(state.last_checked_epoch) < AUTO_CHECK_TTL_SECS
    {
        return;
    }

    let Ok(target) = current_target_triple() else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    let repo = update_repo();
    let base = api_base();

    state.last_checked_epoch = now;
    match fetch_latest_release(&base, &repo, target, Duration::from_secs(3)) {
        Ok(assets) => {
            state.latest_version = assets.version.clone();
            state.last_error.clear();
            write_state(&state_path, &state);

            if !is_newer_version(current, &assets.version) {
                return;
            }
            let notify_only = std::env::var("BERTH_AUTO_UPDATE")
                .map(|v| v.eq_ignore_ascii_case("notify"))
                .unwrap_or(false);
            if notify_only {
                eprintln!(
                    "berth: new version v{} available (current v{}); run `bh update` to upgrade.",
                    assets.version.trim_start_matches('v'),
                    current
                );
                return;
            }
            if let Ok(exe_path) = resolve_current_exe() {
                match download_verify_and_install(
                    &assets,
                    &repo,
                    &exe_path,
                    Duration::from_secs(20),
                ) {
                    Ok(_) => {
                        eprintln!(
                            "berth: auto-updated v{} -> v{} (active on next command)",
                            current,
                            assets.version.trim_start_matches('v')
                        );
                    }
                    Err(err) => {
                        state.last_error = err;
                        write_state(&state_path, &state);
                    }
                }
            }
        }
        Err(err) => {
            state.last_error = err;
            write_state(&state_path, &state);
        }
    }
}

fn auto_update_disabled() -> bool {
    for key in ["BERTH_NO_AUTO_UPDATE", "CI"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false") {
                return true;
            }
        }
    }
    if let Ok(val) = std::env::var("BERTH_AUTO_UPDATE") {
        let lower = val.trim().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "0" | "false" | "off" | "disable" | "disabled"
        ) {
            return true;
        }
    }
    false
}

fn resolve_current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("resolve current executable: {e}"))?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

pub fn fetch_latest_release(
    api_base: &str,
    repo: &str,
    target: &str,
    timeout: Duration,
) -> Result<ReleaseAssets, String> {
    let url = format!(
        "{}/repos/{}/releases/latest",
        api_base.trim_end_matches('/'),
        repo
    );
    let mut headers = vec![
        (
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        (
            "User-Agent".to_string(),
            format!("berth-cli/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    if let Some(tok) = github_token() {
        headers.push(("Authorization".to_string(), format!("Bearer {tok}")));
    }
    let body = http_get_bytes(&url, &headers, timeout, MAX_META_BYTES)?;
    let rel: GithubRelease =
        serde_json::from_slice(&body).map_err(|e| format!("parse GitHub release JSON: {e}"))?;

    let archive_name = format!("berth-{target}.tar.xz");
    let sha_name = format!("{archive_name}.sha256");
    let sig_name = format!("{archive_name}.sigstore.json");

    let archive_url = rel
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .map(|a| a.browser_download_url.clone())
        .ok_or_else(|| format!("release {} missing asset {archive_name}", rel.tag_name))?;
    let sha256_url = rel
        .assets
        .iter()
        .find(|a| a.name == sha_name)
        .map(|a| a.browser_download_url.clone())
        .ok_or_else(|| format!("release {} missing checksum asset {sha_name}", rel.tag_name))?;
    let sigstore_url = rel
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .map(|a| a.browser_download_url.clone());

    Ok(ReleaseAssets {
        version: rel.tag_name.trim_start_matches('v').to_string(),
        target: target.to_string(),
        archive_url,
        sha256_url,
        sigstore_url,
    })
}

pub fn download_verify_and_install(
    assets: &ReleaseAssets,
    repo: &str,
    exe_path: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let ua = vec![(
        "User-Agent".to_string(),
        format!("berth-cli/{}", env!("CARGO_PKG_VERSION")),
    )];
    let sha_bytes = http_get_bytes(&assets.sha256_url, &ua, timeout, MAX_META_BYTES)?;
    let sha_text = String::from_utf8_lossy(&sha_bytes);
    let expected_sha = parse_sha256_file(&sha_text)?;

    let archive_bytes = http_get_bytes(&assets.archive_url, &ua, timeout, MAX_ARCHIVE_BYTES)?;
    let actual_sha = format!("{:x}", Sha256::digest(&archive_bytes));
    if actual_sha != expected_sha {
        return Err(format!(
            "SHA-256 mismatch for {}: expected {expected_sha}, got {actual_sha}",
            assets.archive_url
        ));
    }

    let parent_dir = exe_path.parent().ok_or_else(|| {
        format!(
            "cannot determine parent directory of {}",
            exe_path.display()
        )
    })?;
    let tmp_dir = tempfile::Builder::new()
        .prefix(".berth-update-")
        .tempdir_in(parent_dir)
        .map_err(|e| format!("create temp dir in {}: {e}", parent_dir.display()))?;

    let archive_path = tmp_dir.path().join("berth.tar.xz");
    fs::write(&archive_path, &archive_bytes)
        .map_err(|e| format!("write downloaded archive: {e}"))?;

    // Optional Cosign verification when `cosign` is installed locally.
    let skip_cosign = std::env::var("BERTH_UPDATE_SKIP_COSIGN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !skip_cosign
        && let Some(sig_url) = assets.sigstore_url.as_deref()
        && which::which("cosign").is_ok()
    {
        let sig_bytes = http_get_bytes(sig_url, &ua, timeout, MAX_META_BYTES)?;
        let sig_path = tmp_dir.path().join("berth.tar.xz.sigstore.json");
        fs::write(&sig_path, &sig_bytes).map_err(|e| format!("write sigstore bundle: {e}"))?;
        let identity =
            format!("^https://github.com/{repo}/.github/workflows/dist.yml@refs/tags/.*$");
        let status = Command::new("cosign")
            .args([
                "verify-blob",
                "--bundle",
                sig_path.to_str().unwrap_or(""),
                "--certificate-identity-regexp",
                &identity,
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                archive_path.to_str().unwrap_or(""),
            ])
            .output()
            .map_err(|e| format!("run cosign verify-blob: {e}"))?;
        if !status.status.success() {
            return Err(format!(
                "cosign signature verification failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
    }

    let extract_status = Command::new("tar")
        .args(["-xJf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(tmp_dir.path())
        .arg("berth")
        .output()
        .map_err(|e| format!("extract archive via tar: {e}"))?;
    if !extract_status.status.success() {
        return Err(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&extract_status.stderr).trim()
        ));
    }

    let candidate_path = tmp_dir.path().join("berth");
    #[cfg(unix)]
    fs::set_permissions(&candidate_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod extracted binary: {e}"))?;

    // Smoke-check the candidate binary before replacing the live executable.
    let smoke = Command::new(&candidate_path)
        .arg("--version")
        .output()
        .map_err(|e| format!("smoke-check candidate binary: {e}"))?;
    if !smoke.status.success() {
        return Err(format!(
            "candidate binary failed --version smoke check (status {})",
            smoke.status
        ));
    }

    fs::rename(&candidate_path, exe_path).map_err(|e| {
        format!(
            "replace executable {} -> {}: {e}",
            candidate_path.display(),
            exe_path.display()
        )
    })?;

    Ok(actual_sha)
}

#[derive(Debug)]
struct ParsedUrl {
    https: bool,
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_http_url(raw: &str) -> Result<ParsedUrl, String> {
    let (https, rest) = if let Some(r) = raw.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = raw.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("unsupported URL scheme: {raw}"));
    };
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = if let Some((h, p)) = authority.rsplit_once(':') {
        let port = p
            .parse::<u16>()
            .map_err(|_| format!("invalid port in URL: {raw}"))?;
        (h.to_string(), port)
    } else {
        (authority.to_string(), if https { 443 } else { 80 })
    };
    if host.is_empty() {
        return Err(format!("empty host in URL: {raw}"));
    }
    Ok(ParsedUrl {
        https,
        host,
        port,
        path_and_query,
    })
}

fn http_get_bytes(
    initial_url: &str,
    headers: &[(String, String)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut current_url = initial_url.to_string();
    let mut cur_headers = headers.to_vec();

    for _ in 0..=MAX_REDIRECTS {
        let parsed = parse_http_url(&current_url)?;
        let (status, location, body) = http_get_once(&parsed, &cur_headers, timeout, max_bytes)?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = location.ok_or_else(|| format!("HTTP {status} without Location header"))?;
            // Strip Authorization header on cross-origin redirects (e.g., api.github.com -> objects.githubusercontent.com).
            let next_url = if loc.starts_with("http://") || loc.starts_with("https://") {
                loc
            } else {
                let scheme = if parsed.https { "https" } else { "http" };
                format!("{scheme}://{}:{}{loc}", parsed.host, parsed.port)
            };
            if let Ok(next_parsed) = parse_http_url(&next_url)
                && next_parsed.host != parsed.host
            {
                cur_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Authorization"));
            }
            current_url = next_url;
            continue;
        }
        if status != 200 {
            return Err(format!("GET {current_url} returned HTTP {status}"));
        }
        return Ok(body);
    }
    Err(format!("too many redirects fetching {initial_url}"))
}

fn http_get_once(
    url: &ParsedUrl,
    headers: &[(String, String)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<(u16, Option<String>, Vec<u8>), String> {
    let addr = format!("{}:{}", url.host, url.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}:{}: {e}", url.host, url.port))?
        .next()
        .ok_or_else(|| format!("no address resolved for {}:{}", url.host, url.port))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect {}:{}: {e}", url.host, url.port))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n",
        url.path_and_query, url.host
    );
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");

    if url.https {
        let tls_cfg = build_tls_client_config().map_err(|e| e.to_string())?;
        let server_name = ServerName::try_from(url.host.clone())
            .map_err(|_| format!("invalid TLS server name: {}", url.host))?;
        let conn =
            ClientConnection::new(tls_cfg, server_name).map_err(|e| format!("TLS init: {e}"))?;
        let mut tls = StreamOwned::new(conn, stream);
        tls.write_all(req.as_bytes())
            .map_err(|e| format!("TLS write: {e}"))?;
        tls.flush().map_err(|e| format!("TLS flush: {e}"))?;
        read_http_response(&mut tls, max_bytes)
    } else {
        let mut tcp = stream;
        tcp.write_all(req.as_bytes())
            .map_err(|e| format!("HTTP write: {e}"))?;
        tcp.flush().map_err(|e| format!("HTTP flush: {e}"))?;
        read_http_response(&mut tcp, max_bytes)
    }
}

fn read_http_response<R: Read>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<(u16, Option<String>, Vec<u8>), String> {
    let mut br = BufReader::new(reader);
    let mut status_line = String::new();
    br.read_line(&mut status_line)
        .map_err(|e| format!("read HTTP status line: {e}"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed HTTP status line: {status_line:?}"))?;

    let mut location: Option<String> = None;
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let n = br
            .read_line(&mut line)
            .map_err(|e| format!("read HTTP header: {e}"))?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("location") {
                location = Some(val.to_string());
            } else if key.eq_ignore_ascii_case("transfer-encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            } else if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse::<usize>().ok();
            }
        }
    }

    let mut body = Vec::new();
    if is_chunked {
        loop {
            let mut size_line = String::new();
            br.read_line(&mut size_line)
                .map_err(|e| format!("read chunk size: {e}"))?;
            let hex_part = size_line.split(';').next().unwrap_or("").trim();
            let chunk_len = usize::from_str_radix(hex_part, 16)
                .map_err(|e| format!("invalid chunk size {hex_part:?}: {e}"))?;
            if chunk_len == 0 {
                break;
            }
            if body.len().saturating_add(chunk_len) > max_bytes {
                return Err(format!("response exceeded max {max_bytes} bytes"));
            }
            let start = body.len();
            body.resize(start + chunk_len, 0);
            br.read_exact(&mut body[start..])
                .map_err(|e| format!("read chunk body: {e}"))?;
            let mut crlf = [0u8; 2];
            let _ = br.read_exact(&mut crlf);
        }
    } else if let Some(len) = content_length {
        if len > max_bytes {
            return Err(format!(
                "response {len} bytes exceeds max {max_bytes} bytes"
            ));
        }
        body.resize(len, 0);
        br.read_exact(&mut body)
            .map_err(|e| format!("read HTTP body: {e}"))?;
    } else {
        let mut limited = br.take((max_bytes + 1) as u64);
        limited
            .read_to_end(&mut body)
            .map_err(|e| format!("read HTTP body: {e}"))?;
        if body.len() > max_bytes {
            return Err(format!("response exceeded max {max_bytes} bytes"));
        }
    }

    Ok((status, location, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn semver_comparison_handles_prefixes_and_multidigit() {
        assert!(is_newer_version("2.0.0", "2.1.0"));
        assert!(is_newer_version("2.0.0", "v2.1.0"));
        assert!(is_newer_version("2.2.0", "2.10.0"));
        assert!(!is_newer_version("2.1.0", "2.1.0"));
        assert!(!is_newer_version("2.1.0", "v2.0.9"));
    }

    #[test]
    fn sha256_file_parser_validates_hex_length() {
        let valid =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  berth.tar.xz\n";
        assert_eq!(
            parse_sha256_file(valid).unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(parse_sha256_file("not-a-hash").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn download_verify_and_install_verifies_sha256_and_swaps_atomically() {
        // Build a tiny real .tar.xz archive containing a runnable `berth` script.
        let src_dir = tempfile::tempdir().unwrap();
        let script_path = src_dir.path().join("berth");
        fs::write(&script_path, "#!/bin/sh\necho 'berth 2.1.0'\n").unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let archive_path = src_dir.path().join("berth.tar.xz");
        let status = Command::new("tar")
            .args(["-cJf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(src_dir.path())
            .arg("berth")
            .status()
            .unwrap();
        assert!(status.success());
        let archive_bytes = fs::read(&archive_path).unwrap();
        let good_sha = format!("{:x}", Sha256::digest(&archive_bytes));
        let bad_sha = "a".repeat(64);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let archive_clone = archive_bytes.clone();
        let good_sha_clone = good_sha.clone();
        let bad_sha_clone = bad_sha.clone();

        let handle = thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let body: Vec<u8> = match path {
                    "/good.sha256" => format!("{good_sha_clone}  berth.tar.xz\n").into_bytes(),
                    "/bad.sha256" => format!("{bad_sha_clone}  berth.tar.xz\n").into_bytes(),
                    "/berth.tar.xz" => archive_clone.clone(),
                    _ => b"not found".to_vec(),
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });

        let target_dir = tempfile::tempdir().unwrap();
        let exe_path = target_dir.path().join("berth");
        fs::write(&exe_path, "#!/bin/sh\necho 'berth 2.0.0'\n").unwrap();
        fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755)).unwrap();

        // 1. Bad SHA-256 must fail closed and leave original executable untouched!
        let bad_assets = ReleaseAssets {
            version: "2.1.0".to_string(),
            target: "x86_64-unknown-linux-musl".to_string(),
            archive_url: format!("http://127.0.0.1:{port}/berth.tar.xz"),
            sha256_url: format!("http://127.0.0.1:{port}/bad.sha256"),
            sigstore_url: None,
        };
        assert!(
            download_verify_and_install(
                &bad_assets,
                DEFAULT_UPDATE_REPO,
                &exe_path,
                Duration::from_secs(5),
            )
            .is_err()
        );
        assert!(fs::read_to_string(&exe_path).unwrap().contains("2.0.0"));

        // 2. Good SHA-256 must extract, smoke-check, and swap atomically!
        let good_assets = ReleaseAssets {
            version: "2.1.0".to_string(),
            target: "x86_64-unknown-linux-musl".to_string(),
            archive_url: format!("http://127.0.0.1:{port}/berth.tar.xz"),
            sha256_url: format!("http://127.0.0.1:{port}/good.sha256"),
            sigstore_url: None,
        };
        let installed_sha = download_verify_and_install(
            &good_assets,
            DEFAULT_UPDATE_REPO,
            &exe_path,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(installed_sha, good_sha);
        assert!(fs::read_to_string(&exe_path).unwrap().contains("2.1.0"));

        handle.join().unwrap();
    }
}
