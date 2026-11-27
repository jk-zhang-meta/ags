//! Cross-platform zero-VM native OS sandbox for Berth CLI (`bh` / `berth`).
//!
//! Enforces three unified security guarantees across Linux/WSL2, macOS, and Windows
//! without requiring a VM or Docker daemon:
//! 1. **Zero Side-Channel Network Leak (Fail-Closed)**:
//!    - Linux / WSL2: `bwrap --unshare-net` (empty network namespace — zero host NICs,
//!      zero IPv6/UDP/DNS leak; bridges loopback `127.0.0.1` over private `0600` Unix
//!      Domain Sockets to host-side `CageShim` and `ForwardProxy`).
//!    - macOS: native Seatbelt (`sandbox-exec`) with `(deny network*)`, allowing ONLY
//!      `localhost:<shim_port>` and `localhost:<proxy_port>` and blocking `mDNSResponder`.
//!    - Windows: native AppContainer (`Berth.Cage.Sandbox`) with ZERO network capabilities
//!      (`CapabilityCount = 0`), blocking all direct internet TCP/UDP/IPv6/DNS while
//!      routing all proxy-aware traffic through `127.0.0.1:<proxy_port>`.
//! 2. **Per-Account Isolated Home (`$HOME` / `USERPROFILE`)**:
//!    - Prevents `~/.claude.json` (`userID` / `anonymousId`), `~/.claude/`, and `~/.codex/`
//!      cross-account correlation across account switches.
//!    - Hides host `~/.ssh`, `~/.berth`, `~/.gnupg`, `~/.aws`, `~/.kube`, and WSL `/run/WSL`.
//! 3. **Native Full-Speed Local Workspace Access**:
//!    - Mounts/grants read-write access to `$PWD` at its exact original path.

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
use std::net::SocketAddr;
#[cfg(unix)]
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};
#[cfg(unix)]
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::berth::{
    BerthError, CliLaunchEnv, LeaseCageProfile, write_codex_overlay_config_toml,
    write_private_file_atomic,
};
use crate::cage_shim::CageShimHandle;

pub const WINDOWS_APPCONTAINER_NAME: &str = "Berth.Cage.Sandbox";
#[cfg(unix)]
const TRAMPOLINE_SHIM_TOKEN_ENV: &str = "BERTH_TRAMPOLINE_SHIM_TOKEN";

/// Returns the isolated per-account Windows AppContainer profile name so each
/// account cage has its own SID and cannot read or write sibling account cages.
pub fn windows_appcontainer_profile_name(account_key: &str) -> String {
    format!("{WINDOWS_APPCONTAINER_NAME}.{}", sanitize_account_key(account_key))
}

/// Exact Win32 `CommandLineToArgvW` argument quoting (handles spaces, tabs, quotes,
/// and 2*N trailing backslashes before quotes).
pub fn quote_win32_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"')) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(ch);
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Validates that `workspace_dir` will not expose the user's real `$HOME` or the
/// Berth state/cages directory inside a sandboxed agent.
pub fn validate_sandbox_workspace(
    workspace_dir: &Path,
    real_home: &Path,
    state_dir: &Path,
) -> Result<(), BerthError> {
    let norm = |p: &Path| -> PathBuf {
        if let Ok(c) = fs::canonicalize(p) {
            return c;
        }
        // Walk up to the deepest existing ancestor so symlinks like macOS
        // `/var` -> `/private/var` or `/tmp` -> `/private/tmp` are resolved
        // even when `p` itself does not exist yet.
        let mut suffix = Vec::new();
        let mut cur = p;
        while let Some(parent) = cur.parent() {
            if let Some(name) = cur.file_name() {
                suffix.push(name.to_os_string());
            }
            if let Ok(mut base) = fs::canonicalize(parent) {
                for part in suffix.into_iter().rev() {
                    base.push(part);
                }
                return base;
            }
            cur = parent;
        }
        p.to_path_buf()
    };
    let ws = norm(workspace_dir);
    let home = norm(real_home);
    let state = norm(state_dir);

    if ws == home || home.starts_with(&ws) {
        return Err(BerthError::InvalidArgument(format!(
            "refusing to mount `{}` as sandbox workspace because it exposes your real home directory `{}`; cd into a specific project directory instead",
            workspace_dir.display(),
            real_home.display()
        )));
    }
    if ws == state || state.starts_with(&ws) || ws.starts_with(&state) {
        return Err(BerthError::InvalidArgument(format!(
            "refusing to mount `{}` as sandbox workspace because it overlaps Berth's isolated state/cages directory `{}`",
            workspace_dir.display(),
            state_dir.display()
        )));
    }
    Ok(())
}

/// Removes sensitive host environment variables before launching a sandboxed agent.
pub fn scrub_sensitive_host_env(cmd: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let Some(key) = k.to_str() else {
            continue;
        };
        if is_sensitive_host_env_key(key) {
            cmd.env_remove(key);
        }
    }
}

fn is_sensitive_host_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "SSH_AUTH_SOCK"
            | "SSH_AGENT_PID"
            | "WSL_INTEROP"
            | "WSL_DISTRO_NAME"
            | "WSLENV"
            | "WT_SESSION"
            | "WT_PROFILE_ID"
            | "GPG_AGENT_INFO"
    ) {
        return true;
    }
    if upper.starts_with("BERTH_")
        || upper.starts_with("AGS_")
        || upper.starts_with("AWS_")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GH_")
        || upper.starts_with("OPENAI_")
        || upper.starts_with("ANTHROPIC_")
        || upper.starts_with("CLAUDE_")
        || upper.starts_with("CODEX_")
        || upper.starts_with("GEMINI_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("GROK_")
        || upper.starts_with("XAI_")
    {
        return true;
    }
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("API_KEY")
        || upper.contains("PRIVATE_KEY")
        || upper.contains("CREDENTIAL")
}

fn ensure_dir_no_symlink(path: &Path) -> Result<(), BerthError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(BerthError::Io(format!(
                "refusing to traverse symlink inside cage directory: {}",
                path.display()
            )));
        }
        if meta.is_dir() {
            return Ok(());
        }
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Detected native sandbox backend for the current OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProbe {
    pub os: String,
    pub is_wsl: bool,
    pub backend: String,
    pub available: bool,
    pub kernel_egress_blocked: bool,
    pub localtime_mount_override: bool,
    pub notes: Vec<String>,
}

/// Persistent per-account isolated home & synthetic device-posture files.
#[derive(Debug, Clone)]
pub struct AccountCageHome {
    pub account_key: String,
    pub root_dir: PathBuf,
    pub home_dir: PathBuf,
    pub etc_dir: PathBuf,
    pub machine_id_file: PathBuf,
    pub timezone_file: PathBuf,
    pub locale_file: PathBuf,
    pub passwd_file: PathBuf,
    pub resolv_conf_file: PathBuf,
    pub hostname_file: PathBuf,
    pub hosts_file: PathBuf,
    pub hostname: String,
    pub machine_id: String,
}

/// Parameters for launching an agent binary inside the cross-platform Berth sandbox.
#[derive(Debug, Clone)]
pub struct SandboxLaunchSpec {
    pub agent: String,
    pub workspace_dir: PathBuf,
    pub real_home: PathBuf,
    pub state_dir: PathBuf,
    pub account_home: AccountCageHome,
    pub cage: LeaseCageProfile,
    pub stock_bin: PathBuf,
    pub agent_args: Vec<String>,
    pub launch_env: CliLaunchEnv,
    pub overlay_unset_env: Vec<String>,
    pub overlay_set_env: BTreeMap<String, String>,
    pub overlay_fs_etc: Option<PathBuf>,
    pub allow_env_fallback: bool,
    pub auto_rebind: bool,
}

/// Waits for a spawned CLI child process, or — when `auto_rebind` is enabled
/// and `shim` intercepts `409 cage_rebind_required` — allows a short 350ms
/// transcript-flush window and then cleanly terminates the old CLI process so
/// `bh` can immediately migrate state into the new Cage without waiting for
/// manual user exit or 30s SDK retry backoff.
pub fn wait_child_or_rebind(
    mut child: Child,
    shim: &CageShimHandle,
    auto_rebind: bool,
) -> std::io::Result<ExitStatus> {
    if !auto_rebind {
        return child.wait();
    }
    let mut rebind_seen_at: Option<Instant> = None;
    let mut sigterm_sent_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if shim.has_rebind_signal() {
            let first_seen = *rebind_seen_at.get_or_insert_with(Instant::now);
            // Allow 350ms for CLI to flush last tool_result / JSONL line to disk.
            if first_seen.elapsed() >= Duration::from_millis(350) {
                match sigterm_sent_at {
                    None => {
                        #[cfg(unix)]
                        {
                            let _ = Command::new("kill")
                                .args(["-TERM", &child.id().to_string()])
                                .status();
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill();
                        }
                        sigterm_sent_at = Some(Instant::now());
                    }
                    Some(t) if t.elapsed() >= Duration::from_secs(2) => {
                        let _ = child.kill();
                        return child.wait();
                    }
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Probes native zero-VM sandbox capabilities on Linux/WSL2, macOS, or Windows.
pub fn probe_sandbox_backend() -> SandboxProbe {
    let os = std::env::consts::OS.to_string();
    #[cfg(target_os = "linux")]
    let is_wsl = is_wsl_host();

    #[cfg(target_os = "linux")]
    {
        let mut notes = Vec::new();
        let bwrap_path = which::which("bwrap").ok();
        let bwrap_works = bwrap_path
            .as_ref()
            .and_then(|bin| {
                Command::new(bin)
                    .args(["--unshare-net", "--ro-bind", "/", "/", "--", "true"])
                    .status()
                    .ok()
            })
            .map(|s| s.success())
            .unwrap_or(false);

        if bwrap_works {
            if is_wsl {
                notes.push(
                    "WSL2 detected: /run/WSL, unrelated /mnt/* mounts, WSL_INTEROP, and Windows PATH are stripped inside sandbox".into(),
                );
            }
            return SandboxProbe {
                os,
                is_wsl,
                backend: "linux-bwrap".into(),
                available: true,
                kernel_egress_blocked: true,
                localtime_mount_override: Path::new("/usr/share/zoneinfo").is_dir(),
                notes,
            };
        }

        if bwrap_path.is_none() {
            notes.push(
                "bubblewrap (`bwrap`) not found; install with `sudo pacman -S bubblewrap` (Arch) or `sudo apt install bubblewrap` (Ubuntu/Debian)".into(),
            );
        } else {
            notes.push(
                "`bwrap --unshare-net` failed (unprivileged user/net namespaces may be disabled via kernel.unprivileged_userns_clone or AppArmor)".into(),
            );
        }
        return SandboxProbe {
            os,
            is_wsl,
            backend: "linux-bwrap".into(),
            available: false,
            kernel_egress_blocked: false,
            localtime_mount_override: false,
            notes,
        };
    }

    #[cfg(target_os = "macos")]
    {
        let mut notes = Vec::new();
        let seatbelt_bin = Path::new("/usr/bin/sandbox-exec");
        let works = seatbelt_bin.is_file()
            && Command::new(seatbelt_bin)
                .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if works {
            notes.push(
                "macOS Seatbelt enforces kernel `(deny network*)`, localhost-only shim/proxy egress, mDNSResponder block, and workspace-only writes".into(),
            );
        } else {
            notes.push("/usr/bin/sandbox-exec unavailable on this macOS host".into());
        }
        return SandboxProbe {
            os,
            is_wsl: false,
            backend: "macos-seatbelt".into(),
            available: works,
            kernel_egress_blocked: works,
            localtime_mount_override: false,
            notes,
        };
    }

    #[cfg(target_os = "windows")]
    {
        let mut notes = Vec::new();
        let has_powershell = which::which("powershell.exe").is_ok()
            || which::which("pwsh.exe").is_ok();
        let loopback_ready = windows_loopback_exempt_active();
        if loopback_ready && has_powershell {
            notes.push(
                "Windows AppContainer (`Berth.Cage.Sandbox`) ready with zero network capabilities (blocks direct internet TCP/UDP/IPv6/DNS)".into(),
            );
        } else if has_powershell {
            notes.push(
                "Run once in an elevated terminal: `bh sandbox init-windows` to enable Windows AppContainer loopback bridge for kernel-level egress blocking".into(),
            );
        } else {
            notes.push("powershell.exe not found; Windows AppContainer helper unavailable".into());
        }
        return SandboxProbe {
            os,
            is_wsl: false,
            backend: "windows-appcontainer".into(),
            available: has_powershell && loopback_ready,
            kernel_egress_blocked: has_powershell && loopback_ready,
            localtime_mount_override: false,
            notes,
        };
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        SandboxProbe {
            os,
            is_wsl: false,
            backend: "unsupported".into(),
            available: false,
            kernel_egress_blocked: false,
            localtime_mount_override: false,
            notes: vec!["No native kernel sandbox backend for this OS".into()],
        }
    }
}

pub fn is_wsl_host() -> bool {
    #[cfg(target_os = "linux")]
    {
        let proc_version = fs::read_to_string("/proc/version").unwrap_or_default();
        let osrelease = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        proc_version.to_ascii_lowercase().contains("microsoft")
            || proc_version.to_ascii_lowercase().contains("wsl")
            || osrelease.to_ascii_lowercase().contains("microsoft")
            || osrelease.to_ascii_lowercase().contains("wsl")
            || std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Sanitizes an account/cage key for safe filesystem directory names.
fn sanitize_account_key(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

fn deterministic_hex(seed: &str, len: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(len);
    for b in digest.iter() {
        if out.len() >= len {
            break;
        }
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(len);
    out
}

/// Creates/updates a persistent per-account isolated home directory and synthetic
/// device posture files (`/etc/machine-id`, `/etc/timezone`, `/etc/resolv.conf`).
pub fn prepare_account_cage_home(
    state_dir: &Path,
    account_id: Option<i64>,
    cage: &LeaseCageProfile,
) -> Result<AccountCageHome, BerthError> {
    let raw_key = match account_id {
        Some(id) if id > 0 => format!("acct-{id}"),
        _ => format!(
            "cage-{}-{}",
            cage.cage_id,
            cage.country_code.to_ascii_lowercase()
        ),
    };
    let account_key = sanitize_account_key(&raw_key);
    let cages_root = state_dir.join("cages");
    ensure_dir_no_symlink(&cages_root)?;
    let root_dir = cages_root.join(&account_key);
    let home_dir = root_dir.join("home");
    let etc_dir = root_dir.join("etc");

    for sub in [
        &root_dir,
        &home_dir,
        &etc_dir,
        &home_dir.join(".tmp"),
        &home_dir.join(".run"),
        &home_dir.join(".config"),
        &home_dir.join(".cache"),
        &home_dir.join(".local").join("share"),
        &home_dir.join(".local").join("state"),
        &home_dir.join(".claude"),
        &home_dir.join(".codex"),
        &home_dir.join(".grok"),
        &home_dir.join(".gemini").join("antigravity-cli"),
    ] {
        ensure_dir_no_symlink(sub)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&root_dir, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&home_dir, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(home_dir.join(".tmp"), fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(home_dir.join(".run"), fs::Permissions::from_mode(0o700));
    }

    let machine_id = deterministic_hex(&format!("berth-cage-machine-id:{account_key}"), 32);
    // Antidetect principle: never advertise the cage tool name in os.hostname();
    // use a realistic, deterministic workstation hostname stable across restarts.
    let hostname = format!(
        "dev-{}",
        deterministic_hex(&format!("berth-cage-host:{account_key}"), 6)
    );

    let machine_id_file = etc_dir.join("machine-id");
    let timezone_file = etc_dir.join("timezone");
    let locale_file = etc_dir.join("locale.conf");
    let passwd_file = etc_dir.join("passwd");
    let resolv_conf_file = etc_dir.join("resolv.conf");
    let hostname_file = etc_dir.join("hostname");
    let hosts_file = etc_dir.join("hosts");

    write_private_file_atomic(&machine_id_file, format!("{machine_id}\n").as_bytes())?;
    write_private_file_atomic(&timezone_file, format!("{}\n", cage.timezone).as_bytes())?;
    write_private_file_atomic(
        &locale_file,
        format!("LANG={}\nLC_ALL={}\n", cage.locale, cage.locale).as_bytes(),
    )?;
    let synthetic_passwd = build_minimal_synthetic_passwd();
    write_private_file_atomic(&passwd_file, synthetic_passwd.as_bytes())?;
    write_private_file_atomic(
        &resolv_conf_file,
        b"# Managed local resolver\nnameserver 127.0.0.1\n",
    )?;
    write_private_file_atomic(&hostname_file, format!("{hostname}\n").as_bytes())?;
    write_private_file_atomic(
        &hosts_file,
        format!("127.0.0.1 localhost {hostname}\n").as_bytes(),
    )?;

    // Persist deterministic device persona profile (antidetect sticky binding).
    let profile_path = root_dir.join("profile.json");
    let profile_json = serde_json::json!({
        "schema_version": 1,
        "account_key": account_key,
        "hostname": hostname,
        "machine_id": machine_id,
        "cohort_timezone": cage.timezone,
        "cohort_country": cage.country_code,
        "cohort_locale": cage.locale,
        "cohort_accept_language": cage.accept_language,
        "cage_id": cage.cage_id,
        "gate_epoch": cage.gate_epoch,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&profile_json) {
        let _ = write_private_file_atomic(&profile_path, &bytes);
    }

    Ok(AccountCageHome {
        account_key,
        root_dir,
        home_dir,
        etc_dir,
        machine_id_file,
        timezone_file,
        locale_file,
        passwd_file,
        resolv_conf_file,
        hostname_file,
        hosts_file,
        hostname,
        machine_id,
    })
}

/// Builds a minimal `/etc/passwd` containing only `root`, the current host UID entry
/// (with GECOS real-name field stripped and shell normalized to `/bin/bash`), and `nobody`,
/// preventing enumeration of sibling host user accounts inside Linux `bwrap`.
fn build_minimal_synthetic_passwd() -> String {
    let default_shell = if Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let cur_home = dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/root".to_string());
    let cur_user = std::env::var("USER").unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    if let Ok(raw) = fs::read_to_string("/etc/passwd") {
        for line in raw.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() < 7 {
                continue;
            }
            let uname = parts[0];
            let uid = parts[2];
            let gid = parts[3];
            let home = parts[5];
            let keep = uid == "0"
                || uid == "65534"
                || uname == "nobody"
                || (!cur_user.is_empty() && uname == cur_user)
                || home == cur_home;
            if !keep {
                continue;
            }
            let shell = if uid == "65534" || uname == "nobody" {
                parts[6]
            } else {
                default_shell
            };
            // Strip GECOS real-name field (index 4).
            let entry = format!("{uname}:x:{uid}:{gid}::{home}:{shell}");
            if !lines.contains(&entry) {
                lines.push(entry);
            }
        }
    }
    if lines.is_empty() {
        lines.push(format!("root:x:0:0::/root:{default_shell}"));
        lines.push("nobody:x:65534:65534::/nonexistent:/usr/sbin/nologin".to_string());
    }
    lines.join("\n") + "\n"
}

/// Builds the child environment strictly from an allowlist (`cmd.env_clear()`), preventing
/// leakage of host IDE/desktop/WSL/telemetry/other-agent env vars (`ANTIGRAVITY_*`,
/// `PULSE_SERVER`, `WAYLAND_DISPLAY`, `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `WSL*`,
/// `HOSTNAME`, `ZSH`, `__CF_USER_TEXT_ENCODING`, `AGS_BERTH_*`, `AGENT_SESSION_*`, etc.).
#[cfg(unix)]
fn apply_clean_sandbox_env(
    cmd: &mut Command,
    spec: &SandboxLaunchSpec,
    tmp_dir: &Path,
    runtime_dir: &Path,
) {
    cmd.env_clear();

    // Forward ONLY strictly benign terminal/display formatting preferences.
    const SAFE_HOST_TERM_VARS: &[&str] = &[
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "FORCE_COLOR",
        "COLUMNS",
        "LINES",
        "EDITOR",
        "VISUAL",
        "PAGER",
    ];
    for key in SAFE_HOST_TERM_VARS {
        if let Ok(val) = std::env::var(key) {
            if !val.trim().is_empty() {
                cmd.env(key, val);
            }
        }
    }
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }

    let shell = if Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    cmd.env("SHELL", shell);
    cmd.env("HOSTNAME", &spec.account_home.hostname);
    cmd.env("TMPDIR", tmp_dir);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);

    for (key, value) in &spec.overlay_set_env {
        // Never allow overlay to re-inject berth/cage marker env vars.
        if !key.starts_with("AGS_BERTH_") && !key.starts_with("BERTH_") {
            cmd.env(key, value);
        }
    }
    for key in &spec.overlay_unset_env {
        cmd.env_remove(key);
    }

    spec.launch_env.apply_to_command(cmd);

    // Antidetect: never advertise the cage tool name inside the sandboxed AI CLI's environment.
    for marker in [
        "AGS_BERTH_CAGE_TZ",
        "AGS_BERTH_CAGE_COUNTRY",
        "AGS_BERTH_CAGE_LOCALE",
        "AGS_BERTH_ISOLATION_MODE",
    ] {
        cmd.env_remove(marker);
    }
    // Keep Cage hostname authoritative even if overlay set another HOSTNAME.
    cmd.env("HOSTNAME", &spec.account_home.hostname);
}

/// Writes Codex `model_provider = "berth"` + `[model_providers.berth]` + `[shell_environment_policy]`
/// into the isolated per-account `$HOME/.codex/config.toml` so `codex` sends Responses API SSE requests
/// strictly to the loopback `CageShim` while subshells & MCP tools inherit Cage TZ/locale + ForwardProxy.
pub fn write_codex_sandbox_config(
    account_home: &AccountCageHome,
    cage: &LeaseCageProfile,
    shim_url: &str,
    http_proxy_url: &str,
    socks5_proxy_url: &str,
    workspace_dir: Option<&Path>,
) -> Result<PathBuf, BerthError> {
    let quote = |raw: &str| -> String {
        let esc = raw
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("\"{esc}\"")
    };
    let base_v1 = format!("{}/v1", shim_url.trim_end_matches('/'));
    let trust_block = workspace_dir
        .map(|ws| {
            format!(
                "\n[projects.{}]\ntrust_level = \"trusted\"\n",
                quote(&ws.to_string_lossy())
            )
        })
        .unwrap_or_default();
    let snippet = format!(
        "model_provider = \"berth\"\n\n\
         [features]\n\
         daemon_auto_start = false\n\n\
         [model_providers.berth]\n\
         name = \"Berth\"\n\
         base_url = {}\n\
         env_key = \"OPENAI_API_KEY\"\n\
         wire_api = \"responses\"\n\
         supports_websockets = false\n\n\
         [shell_environment_policy]\ninherit = \"core\"\n\n[shell_environment_policy.set]\n\
         TZ = {}\nLANG = {}\nLC_ALL = {}\nLC_TIME = {}\n\
         HTTP_PROXY = {}\nHTTPS_PROXY = {}\nALL_PROXY = {}\n\
         http_proxy = {}\nhttps_proxy = {}\nall_proxy = {}\n\
         NO_PROXY = \"127.0.0.1,localhost\"\nno_proxy = \"127.0.0.1,localhost\"\n{}",
        quote(&base_v1),
        quote(&cage.timezone),
        quote(&cage.locale),
        quote(&cage.locale),
        quote(&cage.locale),
        quote(http_proxy_url),
        quote(http_proxy_url),
        quote(socks5_proxy_url),
        quote(http_proxy_url),
        quote(http_proxy_url),
        quote(socks5_proxy_url),
        trust_block,
    );
    write_codex_overlay_config_toml(&account_home.home_dir.join(".codex"), &snippet)
}

/// Ensures the isolated per-account `$CLAUDE_CONFIG_DIR/.claude.json` has
/// first-run UI onboarding and current-workspace trust pre-accepted so
/// interactive `bh claude` (including seamless cross-cage `--auto-rebind`)
/// never stalls on theme/trust wizards in a brand-new account Cage.
///
/// Security guarantee: NEVER copies `machineID`, `userID`, `anonymousId`, or
/// credentials from the host or another account Cage; Claude generates fresh
/// per-account identity fields inside each isolated Cage.
pub fn write_claude_sandbox_config(
    account_home: &AccountCageHome,
    workspace_dir: &Path,
) -> Result<PathBuf, BerthError> {
    let claude_dir = account_home.home_dir.join(".claude");
    fs::create_dir_all(&claude_dir)?;
    let cfg_path = claude_dir.join(".claude.json");
    if cfg_path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(BerthError::InvalidArgument(format!(
            "refusing to write symlinked Claude config {}",
            cfg_path.display()
        )));
    }
    let mut root: serde_json::Map<String, serde_json::Value> = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    root.entry("hasCompletedOnboarding".to_string())
        .or_insert(serde_json::Value::Bool(true));
    root.entry("theme".to_string())
        .or_insert_with(|| serde_json::Value::String("dark".to_string()));
    let projects = root
        .entry("projects".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !projects.is_object() {
        *projects = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(proj_map) = projects.as_object_mut() {
        let mut keys = vec![workspace_dir.to_string_lossy().into_owned()];
        if let Ok(canon) = fs::canonicalize(workspace_dir) {
            let cs = canon.to_string_lossy().into_owned();
            if !keys.contains(&cs) {
                keys.push(cs);
            }
        }
        for key in keys {
            let entry = proj_map
                .entry(key)
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(obj) = entry.as_object_mut() {
                obj.insert(
                    "hasTrustDialogAccepted".to_string(),
                    serde_json::Value::Bool(true),
                );
                obj.insert(
                    "hasCompletedProjectOnboarding".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .map_err(|e| BerthError::Io(e.to_string()))?;
    let tmp_path = claude_dir.join(format!(".claude.json.tmp-{}", uuid::Uuid::new_v4().simple()));
    fs::write(&tmp_path, body)?;
    fs::rename(&tmp_path, &cfg_path)?;
    Ok(cfg_path)
}

/// Applies isolated per-account HOME/USERPROFILE + unified Berth ForwardProxy env vars.
pub fn apply_proxy_and_home_env(
    cmd: &mut Command,
    account_home: &AccountCageHome,
    http_proxy_url: &str,
    socks5_proxy_url: &str,
) {
    cmd.env("HOME", &account_home.home_dir);
    cmd.env("XDG_CONFIG_HOME", account_home.home_dir.join(".config"));
    cmd.env("XDG_CACHE_HOME", account_home.home_dir.join(".cache"));
    cmd.env(
        "XDG_DATA_HOME",
        account_home.home_dir.join(".local").join("share"),
    );
    cmd.env(
        "XDG_STATE_HOME",
        account_home.home_dir.join(".local").join("state"),
    );
    cmd.env("CODEX_HOME", account_home.home_dir.join(".codex"));
    cmd.env("CLAUDE_CONFIG_DIR", account_home.home_dir.join(".claude"));
    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", &account_home.home_dir);
        cmd.env("APPDATA", account_home.home_dir.join("AppData").join("Roaming"));
        cmd.env("LOCALAPPDATA", account_home.home_dir.join("AppData").join("Local"));
    }

    cmd.env("HTTP_PROXY", http_proxy_url);
    cmd.env("HTTPS_PROXY", http_proxy_url);
    cmd.env("ALL_PROXY", socks5_proxy_url);
    cmd.env("http_proxy", http_proxy_url);
    cmd.env("https_proxy", http_proxy_url);
    cmd.env("all_proxy", socks5_proxy_url);
    cmd.env("NO_PROXY", "127.0.0.1,localhost");
    cmd.env("no_proxy", "127.0.0.1,localhost");
}

/// Launches the stock CLI inside the OS-native zero-VM sandbox (Linux/WSL2 `bwrap`,
/// macOS `sandbox-exec`, or Windows `AppContainer`), routing ALL outbound network traffic
/// through `shim`'s `CageShim` + `ForwardProxy` -> Berth Landing Node.
pub fn launch_in_sandbox(
    spec: &SandboxLaunchSpec,
    shim: &mut CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    validate_sandbox_workspace(&spec.workspace_dir, &spec.real_home, &spec.state_dir)?;
    if spec.agent == "claude" {
        let _ = write_claude_sandbox_config(&spec.account_home, &spec.workspace_dir);
    }
    let probe = probe_sandbox_backend();

    #[cfg(target_os = "linux")]
    {
        if probe.available {
            return launch_linux_bwrap(spec, shim);
        }
        if !spec.allow_env_fallback {
            return Err(BerthError::StrictIsolationUnavailable(
                probe.notes.join("; "),
            ));
        }
        eprintln!(
            "[berth] warning: {} — falling back to isolated-HOME + full-proxy mode",
            probe.notes.join("; ")
        );
        return launch_env_with_proxy(spec, shim);
    }

    #[cfg(target_os = "macos")]
    {
        if probe.available {
            return launch_macos_seatbelt(spec, shim);
        }
        if !spec.allow_env_fallback {
            return Err(BerthError::StrictIsolationUnavailable(
                probe.notes.join("; "),
            ));
        }
        return launch_env_with_proxy(spec, shim);
    }

    #[cfg(target_os = "windows")]
    {
        if probe.available {
            return launch_windows_appcontainer(spec, shim);
        }
        if !spec.allow_env_fallback {
            return Err(BerthError::StrictIsolationUnavailable(
                probe.notes.join("; "),
            ));
        }
        eprintln!(
            "[berth] notice: {} — running with isolated USERPROFILE + full-proxy mode",
            probe.notes.join("; ")
        );
        return launch_env_with_proxy(spec, shim);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        launch_env_with_proxy(spec, shim)
    }
}

fn launch_env_with_proxy(
    spec: &SandboxLaunchSpec,
    shim: &CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    if spec.agent == "codex" {
        let _ = write_codex_sandbox_config(
            &spec.account_home,
            &spec.cage,
            &shim.listen_url(),
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
            Some(&spec.workspace_dir),
        );
    }
    let mut cmd = Command::new(&spec.stock_bin);
    cmd.args(&spec.agent_args);
    cmd.current_dir(&spec.workspace_dir);
    scrub_sensitive_host_env(&mut cmd);
    for key in &spec.overlay_unset_env {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.overlay_set_env {
        cmd.env(key, value);
    }
    if let Some(etc) = &spec.overlay_fs_etc {
        cmd.env("AGS_FS_OVERLAY", etc);
    }
    spec.launch_env.apply_to_command(&mut cmd);
    apply_proxy_and_home_env(
        &mut cmd,
        &spec.account_home,
        &shim.proxy_http_url(),
        &shim.proxy_socks5_url(),
    );
    let child = cmd.spawn()?;
    Ok(wait_child_or_rebind(child, shim, spec.auto_rebind)?)
}

// ---------------------------------------------------------------------------
// Backend 1: Linux / WSL2 — bubblewrap (`bwrap --unshare-net`) + Unix Socket Trampoline
// ---------------------------------------------------------------------------

/// Builds the complete `bwrap` argv for a zero-leak Linux/WSL2 sandbox.
/// Exposed publicly for deterministic unit testing across Linux/macOS/Windows.
pub fn build_bwrap_args(
    workspace_dir: &Path,
    real_home: &Path,
    account_home: &AccountCageHome,
    cage_tz: &str,
    bridge_dir: &Path,
    bh_exe: &Path,
    shim_port: u16,
    proxy_port: u16,
    agent: &str,
    stock_bin: &Path,
    agent_args: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--unshare-net".into(),
        "--unshare-uts".into(),
        "--unshare-pid".into(),
        "--die-with-parent".into(),
        "--hostname".into(),
        account_home.hostname.clone(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
    ];

    // Mount standard Linux root directories read-only, respecting Arch/modern merged-usr symlinks.
    // `/etc` is built separately on a tmpfs so symlinked /etc/localtime & /etc/resolv.conf
    // (standard on Ubuntu, Arch, and WSL2) can be replaced cleanly with zero symlink leakage!
    for top in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt", "/var"] {
        let p = Path::new(top);
        match fs::symlink_metadata(p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Ok(target) = fs::read_link(p) {
                    args.push("--symlink".into());
                    args.push(target.to_string_lossy().into_owned());
                    args.push(top.into());
                }
            }
            Ok(meta) if meta.is_dir() => {
                args.push("--ro-bind".into());
                args.push(top.into());
                args.push(top.into());
            }
            _ => {}
        }
    }

    // Synthetic /etc: inherit host /etc entries read-only EXCEPT identity/network posture files.
    args.push("--tmpfs".into());
    args.push("/etc".into());
    if let Ok(entries) = fs::read_dir("/etc") {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| {
                !matches!(
                    name.as_str(),
                    "localtime"
                        | "timezone"
                        | "locale.conf"
                        | "locale.gen"
                        | "default"
                        | "passwd"
                        | "machine-id"
                        | "resolv.conf"
                        | "hostname"
                        | "hosts"
                        | "hosts.allow"
                        | "hosts.deny"
                        | "wsl.conf"
                        | "fstab"
                        | "mtab"
                        | "crypttab"
                        | "NetworkManager"
                        | "netplan"
                        | "systemd"
                        | "tailscale"
                        | "wireguard"
                        | "meshharbor"
                        | "ssh"
                        | "shadow"
                        | "gshadow"
                        | "sudoers"
                        | "sudoers.d"
                )
            })
            .collect();
        names.sort();
        for name in names {
            let host_entry = Path::new("/etc").join(&name);
            let dst = format!("/etc/{name}");
            match fs::symlink_metadata(&host_entry) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    if let Ok(target) = fs::read_link(&host_entry) {
                        args.push("--symlink".into());
                        args.push(target.to_string_lossy().into_owned());
                        args.push(dst);
                    }
                }
                Ok(_) => {
                    args.push("--ro-bind".into());
                    args.push(host_entry.to_string_lossy().into_owned());
                    args.push(dst);
                }
                Err(_) => {}
            }
        }
    }

    // Authoritative Cage /etc posture overrides (clean symlink for /etc/localtime so even readlink sees Cage TZ!).
    let zoneinfo = Path::new("/usr/share/zoneinfo").join(cage_tz);
    if zoneinfo.is_file() {
        args.push("--symlink".into());
        args.push(zoneinfo.to_string_lossy().into_owned());
        args.push("/etc/localtime".into());
    }
    args.push("--ro-bind".into());
    args.push(account_home.timezone_file.to_string_lossy().into_owned());
    args.push("/etc/timezone".into());
    args.push("--ro-bind".into());
    args.push(account_home.locale_file.to_string_lossy().into_owned());
    args.push("/etc/locale.conf".into());
    args.push("--dir".into());
    args.push("/etc/default".into());
    args.push("--ro-bind".into());
    args.push(account_home.locale_file.to_string_lossy().into_owned());
    args.push("/etc/default/locale".into());
    args.push("--ro-bind".into());
    args.push(account_home.passwd_file.to_string_lossy().into_owned());
    args.push("/etc/passwd".into());
    args.push("--ro-bind".into());
    args.push(account_home.machine_id_file.to_string_lossy().into_owned());
    args.push("/etc/machine-id".into());
    args.push("--ro-bind".into());
    args.push(account_home.resolv_conf_file.to_string_lossy().into_owned());
    args.push("/etc/resolv.conf".into());
    args.push("--ro-bind".into());
    args.push(account_home.hostname_file.to_string_lossy().into_owned());
    args.push("/etc/hostname".into());
    args.push("--ro-bind".into());
    args.push(account_home.hosts_file.to_string_lossy().into_owned());
    args.push("/etc/hosts".into());

    if fs::symlink_metadata("/var/lib/dbus/machine-id")
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        args.push("--ro-bind".into());
        args.push(account_home.machine_id_file.to_string_lossy().into_owned());
        args.push("/var/lib/dbus/machine-id".into());
    }

    // Replace real $HOME with persistent isolated per-account cage home (hides ~/.ssh, ~/.berth, etc.).
    let real_home_str = real_home.to_string_lossy().into_owned();
    let cage_home_str = account_home.home_dir.to_string_lossy().into_owned();
    args.push("--bind".into());
    args.push(cage_home_str);
    args.push(real_home_str.clone());

    // Read-only mount safe user developer toolchains (NEVER ~/.ssh, ~/.berth, ~/.claude, ~/.codex, ~/.aws, ~/.gnupg).
    // Include `~/.local/share/claude/versions` and `~/.local/lib/node_modules` so official native
    // Claude Code (`~/.local/bin/claude -> ~/.local/share/claude/versions/<ver>`) and user-prefix
    // npm CLIs resolve cleanly inside the isolated home bind mount without exposing credentials.
    for safe_tool_rel in [
        ".cargo/bin",
        ".rustup",
        ".local/bin",
        ".local/share/claude/versions",
        ".local/lib/node_modules",
        ".nvm",
        ".fnm",
        ".pyenv",
        ".local/share/mise",
        ".npm-global",
        ".bun",
        ".volta",
        "go/bin",
    ] {
        let host_tool = real_home.join(safe_tool_rel);
        if host_tool.exists() {
            let s = host_tool.to_string_lossy().into_owned();
            args.push("--ro-bind".into());
            args.push(s.clone());
            args.push(s);
        }
    }

    // If stock_bin (or its symlink target) lives in an uncommon non-system path (e.g. custom prefix),
    // expose its parent dir or resolved file read-only unless it sits inside sensitive home dirs.
    let is_sensitive_home_path = |p: &Path| -> bool {
        if let Ok(rel) = p.strip_prefix(real_home) {
            if let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
                return matches!(
                    first,
                    ".ssh" | ".berth" | ".claude" | ".codex" | ".aws" | ".gnupg" | ".config"
                );
            }
        }
        false
    };
    if let Some(bin_parent) = stock_bin.parent() {
        let bp_str = bin_parent.to_string_lossy();
        let covered_by_sys = ["/usr", "/bin", "/sbin", "/opt", "/var"]
            .iter()
            .any(|p| bp_str.starts_with(p));
        if !covered_by_sys && bin_parent.is_dir() && !bin_parent.starts_with(real_home) {
            args.push("--ro-bind".into());
            args.push(bp_str.clone().into_owned());
            args.push(bp_str.into_owned());
        }
    }
    if let Ok(resolved_bin) = fs::canonicalize(stock_bin) {
        let rb_str = resolved_bin.to_string_lossy();
        let covered_by_sys = ["/usr", "/bin", "/sbin", "/opt", "/var"]
            .iter()
            .any(|p| rb_str.starts_with(p));
        if resolved_bin != stock_bin
            && !covered_by_sys
            && resolved_bin.is_file()
            && !is_sensitive_home_path(&resolved_bin)
        {
            args.push("--ro-bind".into());
            args.push(rb_str.clone().into_owned());
            args.push(rb_str.into_owned());
        }
    }

    // Bind-mount ONLY the current project workspace read-write at its exact original path!
    // (Works identically for ext4 paths AND WSL /mnt/c/... projects without mounting unrelated /mnt/*.)
    let ws_str = workspace_dir.to_string_lossy().into_owned();
    args.push("--bind".into());
    args.push(ws_str.clone());
    args.push(ws_str.clone());

    // Mount private host Unix socket bridge dir + bh binary into /run/berth.
    args.push("--bind".into());
    args.push(bridge_dir.to_string_lossy().into_owned());
    args.push("/run/berth".into());

    args.push("--ro-bind".into());
    args.push(bh_exe.to_string_lossy().into_owned());
    args.push("/run/berth/bh".into());

    args.push("--chdir".into());
    args.push(ws_str);

    // Explicitly clear WSL Windows interop env vars inside sandbox.
    for wsl_var in [
        "WSL_INTEROP",
        "WSL_DISTRO_NAME",
        "WSLENV",
        "WT_SESSION",
        "WT_PROFILE_ID",
    ] {
        args.push("--unsetenv".into());
        args.push(wsl_var.into());
    }

    args.push("--".into());
    args.push("/run/berth/bh".into());
    args.push("__sandbox-trampoline".into());
    args.push("--shim-sock".into());
    args.push("/run/berth/shim.sock".into());
    args.push("--proxy-sock".into());
    args.push("/run/berth/proxy.sock".into());
    args.push("--shim-port".into());
    args.push(shim_port.to_string());
    args.push("--proxy-port".into());
    args.push(proxy_port.to_string());
    args.push("--agent".into());
    args.push(agent.into());
    args.push("--".into());
    args.push(stock_bin.to_string_lossy().into_owned());
    args.extend(agent_args.iter().cloned());
    args
}

#[cfg(target_os = "linux")]
fn launch_linux_bwrap(
    spec: &SandboxLaunchSpec,
    shim: &mut CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    let bwrap_bin = which::which("bwrap").map_err(|_| {
        BerthError::StrictIsolationUnavailable("bubblewrap (`bwrap`) binary not found".into())
    })?;
    let current_exe = std::env::current_exe()?;
    let bridge_tmp = tempfile::Builder::new()
        .prefix("berth-sandbox-bridge-")
        .tempdir()?;
    let (_shim_sock, _proxy_sock) = shim.start_unix_bridges(bridge_tmp.path())?;

    let bwrap_args = build_bwrap_args(
        &spec.workspace_dir,
        &spec.real_home,
        &spec.account_home,
        &spec.cage.timezone,
        bridge_tmp.path(),
        &current_exe,
        shim.listen_addr().port(),
        shim.proxy_addr().port(),
        &spec.agent,
        &spec.stock_bin,
        &spec.agent_args,
    );

    // Write Codex shell policy with the exact matching sandbox proxy URLs.
    if spec.agent == "codex" {
        let _ = write_codex_sandbox_config(
            &spec.account_home,
            &spec.cage,
            &shim.listen_url(),
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
            Some(&spec.workspace_dir),
        );
    }

    let mut cmd = Command::new(&bwrap_bin);
    cmd.args(&bwrap_args);
    apply_clean_sandbox_env(&mut cmd, spec, Path::new("/tmp"), Path::new("/tmp/runtime"));
    // Inside bwrap, real_home path IS bind-mounted to account_home.home_dir!
    cmd.env("HOME", &spec.real_home);
    cmd.env("XDG_CONFIG_HOME", spec.real_home.join(".config"));
    cmd.env("XDG_CACHE_HOME", spec.real_home.join(".cache"));
    cmd.env("XDG_DATA_HOME", spec.real_home.join(".local").join("share"));
    cmd.env("XDG_STATE_HOME", spec.real_home.join(".local").join("state"));
    cmd.env("CODEX_HOME", spec.real_home.join(".codex"));
    cmd.env("CLAUDE_CONFIG_DIR", spec.real_home.join(".claude"));
    cmd.env(TRAMPOLINE_SHIM_TOKEN_ENV, shim.shim_token());

    let status = cmd
        .spawn()
        .and_then(|child| wait_child_or_rebind(child, shim, spec.auto_rebind));
    // Shut down Unix socket bridges BEFORE bridge_tmp drops and unlinks the socket files!
    shim.stop_unix_bridges();
    Ok(status?)
}

/// Runs INSIDE the Linux/WSL2 `--unshare-net` namespace:
/// 1. Binds `127.0.0.1:<shim_port>` and `127.0.0.1:<proxy_port>` on the isolated `lo` interface
///    (matching the host ports bit-for-bit so CageShim's Host-header control passes cleanly).
/// 2. Bridges raw TCP streams over `/run/berth/shim.sock` & `/run/berth/proxy.sock` to the host.
/// 3. Sets AI provider `BASE_URL` + `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` to those local ports.
/// 4. Spawns the target CLI and propagates its exact exit code.
#[cfg(unix)]
pub fn run_sandbox_trampoline(
    shim_sock: &Path,
    proxy_sock: &Path,
    shim_port: u16,
    proxy_port: u16,
    agent: &str,
    command: &[String],
) -> Result<ExitCode, BerthError> {
    if command.is_empty() {
        return Err(BerthError::InvalidArgument(
            "__sandbox-trampoline requires a command after `--`".into(),
        ));
    }
    let shim_token = std::env::var(TRAMPOLINE_SHIM_TOKEN_ENV).unwrap_or_default();
    if let Ok(rt_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let rt_path = Path::new(&rt_dir);
        let _ = fs::create_dir_all(rt_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(rt_path, fs::Permissions::from_mode(0o700));
        }
    }

    let shim_listener = TcpListener::bind(("127.0.0.1", shim_port))
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))?;
    let local_shim_addr = shim_listener.local_addr()?;
    let proxy_listener = TcpListener::bind(("127.0.0.1", proxy_port))
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))?;
    let local_proxy_addr = proxy_listener.local_addr()?;

    let s_sock = shim_sock.to_path_buf();
    let _t1 = thread::Builder::new()
        .name("bh-trampoline-shim".into())
        .spawn(move || tcp_to_unix_bridge_loop(shim_listener, s_sock));

    let p_sock = proxy_sock.to_path_buf();
    let _t2 = thread::Builder::new()
        .name("bh-trampoline-proxy".into())
        .spawn(move || tcp_to_unix_bridge_loop(proxy_listener, p_sock));

    let base = format!("http://{local_shim_addr}");
    let http_proxy = if shim_token.is_empty() {
        format!("http://{local_proxy_addr}")
    } else {
        format!("http://bh:{shim_token}@{local_proxy_addr}")
    };
    let socks5_proxy = if shim_token.is_empty() {
        format!("socks5h://{local_proxy_addr}")
    } else {
        format!("socks5h://bh:{shim_token}@{local_proxy_addr}")
    };

    let mut child = Command::new(&command[0]);
    child.args(&command[1..]);
    child.env_remove(TRAMPOLINE_SHIM_TOKEN_ENV);

    match agent {
        "codex" => {
            child.env("OPENAI_BASE_URL", format!("{base}/v1"));
        }
        "claude" => {
            child.env("ANTHROPIC_BASE_URL", &base);
        }
        "agy" => {
            child.env("AGY_LLM_GATEWAY_URL", &base);
            child.env("GEMINI_API_BASE", &base);
            child.env("GOOGLE_GEMINI_BASE_URL", &base);
        }
        "grok" => {
            child.env("XAI_BASE_URL", format!("{base}/v1"));
            child.env("GROK_BASE_URL", format!("{base}/v1"));
        }
        _ => {}
    }

    child.env("HTTP_PROXY", &http_proxy);
    child.env("HTTPS_PROXY", &http_proxy);
    child.env("ALL_PROXY", &socks5_proxy);
    child.env("http_proxy", &http_proxy);
    child.env("https_proxy", &http_proxy);
    child.env("all_proxy", &socks5_proxy);
    child.env("NO_PROXY", "127.0.0.1,localhost");
    child.env("no_proxy", "127.0.0.1,localhost");

    let status = child.status()?;
    Ok(status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

#[cfg(not(unix))]
pub fn run_sandbox_trampoline(
    _shim_sock: &Path,
    _proxy_sock: &Path,
    _shim_port: u16,
    _proxy_port: u16,
    _agent: &str,
    _command: &[String],
) -> Result<ExitCode, BerthError> {
    Err(BerthError::InvalidArgument(
        "__sandbox-trampoline is only used on Unix namespaces".into(),
    ))
}

#[cfg(unix)]
fn tcp_to_unix_bridge_loop(listener: TcpListener, unix_path: PathBuf) {
    use std::os::unix::net::UnixStream;

    for incoming in listener.incoming() {
        let Ok(tcp) = incoming else {
            break;
        };
        let target = unix_path.clone();
        let _ = thread::spawn(move || {
            let _ = tcp.set_nodelay(true);
            let Ok(unix) = UnixStream::connect(&target) else {
                let _ = tcp.shutdown(Shutdown::Both);
                return;
            };
            pipe_tcp_and_unix(tcp, unix);
        });
    }
}

#[cfg(unix)]
pub(crate) fn pipe_tcp_and_unix(tcp: TcpStream, unix: std::os::unix::net::UnixStream) {
    let Ok(mut tcp_read) = tcp.try_clone() else {
        return;
    };
    let Ok(mut unix_write) = unix.try_clone() else {
        return;
    };
    let mut tcp_write = tcp;
    let mut unix_read = unix;

    let t = thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = tcp_read.read(&mut buf) {
            if n == 0 {
                break;
            }
            if unix_write.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = unix_write.shutdown(Shutdown::Write);
    });

    let mut buf = [0u8; 16 * 1024];
    while let Ok(n) = unix_read.read(&mut buf) {
        if n == 0 {
            break;
        }
        if tcp_write.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = tcp_write.shutdown(Shutdown::Write);
    let _ = t.join();
}

// ---------------------------------------------------------------------------
// Backend 2: macOS — Native Seatbelt (`sandbox-exec`)
// ---------------------------------------------------------------------------

fn sbpl_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Returns the raw path plus its canonicalized / macOS `/private/*` variants (deduplicated).
/// On macOS, `/tmp`, `/var`, and `/etc` symlink into `/private/*`, and Seatbelt evaluates
/// rules against kernel-resolved real paths.
fn macos_seatbelt_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if let Ok(canon) = fs::canonicalize(path) {
        if !out.contains(&canon) {
            out.push(canon);
        }
    }
    if let Some(s) = path.to_str() {
        for prefix in ["/var/", "/tmp/", "/etc/"] {
            if s.starts_with(prefix) {
                let priv_path = PathBuf::from(format!("/private{s}"));
                if !out.contains(&priv_path) {
                    out.push(priv_path);
                }
            }
        }
        if matches!(s, "/var" | "/tmp" | "/etc") {
            let priv_path = PathBuf::from(format!("/private{s}"));
            if !out.contains(&priv_path) {
                out.push(priv_path);
            }
        }
    }
    out
}

/// Generates a fail-closed macOS Seatbelt (`.sb`) profile enforcing:
/// - `(deny network*)`, allowing ONLY `localhost:<shim_port>` and `localhost:<proxy_port>`
///   and blocking `mDNSResponder`
/// - **Deny-by-default on the entire real `$HOME`** (`(deny file-read* (subpath "<real_home>"))`),
///   eliminating exposure of `~/.claude.json`, `~/.gitconfig`, `~/.zsh_history`, `~/OneDrive`,
///   `~/Library/Preferences`, `~/.ssh`, sibling cages, and any future files in `$HOME`
/// - Blocks host timezone (`/etc/localtime`, `/var/db/timed`), resolver (`/etc/resolv.conf`),
///   system preferences, `cfprefsd`/`timed`/`locationd`/`SystemConfiguration` Mach IPC,
///   cross-process enumeration (`process-info*`), IOKit hardware UUID/serial (`iokit-open`/
///   `iokit-get-properties`), and host-identifying `sysctl` (`kern.hostname`, `kern.uuid`,
///   `kern.boottime`, `kern.proc`, `net.routetable`)
/// - Re-allows reads ONLY inside `$PWD`, the active `<cage_home>`, and narrow safe toolchain dirs
pub fn generate_macos_seatbelt_profile(
    workspace_dir: &Path,
    real_home: &Path,
    state_dir: &Path,
    cage_home: &Path,
    shim_addr: SocketAddr,
    proxy_addr: SocketAddr,
) -> String {
    generate_macos_seatbelt_profile_with_bin(
        workspace_dir,
        real_home,
        state_dir,
        cage_home,
        shim_addr,
        proxy_addr,
        None,
    )
}

pub fn generate_macos_seatbelt_profile_with_bin(
    workspace_dir: &Path,
    real_home: &Path,
    state_dir: &Path,
    cage_home: &Path,
    shim_addr: SocketAddr,
    proxy_addr: SocketAddr,
    stock_bin: Option<&Path>,
) -> String {
    let shim_port = shim_addr.port();
    let proxy_port = proxy_addr.port();

    let cages_root = state_dir.join("cages");
    let mut deny_read_lines = Vec::new();
    // Deny the ENTIRE real home directory by default (catches ~/.claude.json, ~/.gitconfig,
    // ~/.zsh_history, ~/OneDrive, ~/Library/Preferences, ~/.ssh, etc. with zero blind spots).
    for rh in macos_seatbelt_path_variants(real_home) {
        deny_read_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&rh)));
    }
    for cr in macos_seatbelt_path_variants(&cages_root) {
        deny_read_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&cr)));
    }
    for sd in macos_seatbelt_path_variants(state_dir) {
        deny_read_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&sd)));
    }

    let mut allow_cage_read_lines = Vec::new();
    for ch in macos_seatbelt_path_variants(cage_home) {
        allow_cage_read_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&ch)));
    }

    // Collect narrow safe user toolchain read-only paths inside real_home ONLY when needed
    // (never ~/.ssh, ~/.berth, ~/.claude, ~/.claude.json, ~/.codex, ~/.config, ~/Library).
    let mut safe_tool_paths: Vec<PathBuf> = Vec::new();
    for rel in [
        ".local/bin",
        ".local/share/claude/versions",
        ".local/lib/node_modules",
        ".cargo/bin",
        ".rustup",
        ".nvm",
        ".fnm",
        ".pyenv",
        ".local/share/mise",
        ".npm-global",
        ".bun",
        ".volta",
        "go/bin",
    ] {
        let candidate = real_home.join(rel);
        if candidate.exists() {
            safe_tool_paths.push(candidate);
        }
    }
    let is_sensitive_home_path = |p: &Path| -> bool {
        if let Ok(rel) = p.strip_prefix(real_home) {
            if let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
                return matches!(
                    first,
                    ".ssh"
                        | ".berth"
                        | ".claude"
                        | ".claude.json"
                        | ".codex"
                        | ".grok"
                        | ".gemini"
                        | ".aws"
                        | ".gnupg"
                        | ".config"
                        | "Library"
                );
            }
        }
        false
    };
    if let Some(bin) = stock_bin {
        if let Some(parent) = bin.parent() {
            if parent.starts_with(real_home) && !is_sensitive_home_path(parent) && parent.is_dir() {
                safe_tool_paths.push(parent.to_path_buf());
            }
        }
        if let Ok(canon) = fs::canonicalize(bin) {
            if canon.starts_with(real_home) && !is_sensitive_home_path(&canon) {
                if let Some(parent) = canon.parent() {
                    if !is_sensitive_home_path(parent) {
                        safe_tool_paths.push(parent.to_path_buf());
                    }
                }
            }
        }
    }
    for tp in &safe_tool_paths {
        for v in macos_seatbelt_path_variants(tp) {
            let line = format!("  (subpath \"{}\")", sbpl_escape(&v));
            if !allow_cage_read_lines.contains(&line) {
                allow_cage_read_lines.push(line);
            }
        }
    }

    // Allow lstat/realpath traversal (`file-read-metadata` ONLY, NOT `file-read-data` / readdir)
    // on the exact ancestor path literals leading from real_home to cage_home, workspace_dir,
    // and safe toolchain dirs so fs::canonicalize() succeeds without listing real_home!
    let mut allow_ancestor_meta_lines = Vec::new();
    let mut add_ancestors_to = |target: &Path| {
        let mut cur = target.parent();
        while let Some(anc) = cur {
            if anc == Path::new("/") {
                break;
            }
            for variant in macos_seatbelt_path_variants(anc) {
                let line = format!("  (literal \"{}\")", sbpl_escape(&variant));
                if !allow_ancestor_meta_lines.contains(&line) {
                    allow_ancestor_meta_lines.push(line);
                }
            }
            if anc == real_home {
                break;
            }
            cur = anc.parent();
        }
    };
    add_ancestors_to(cage_home);
    if workspace_dir.starts_with(real_home) {
        add_ancestors_to(workspace_dir);
    }
    for tp in &safe_tool_paths {
        add_ancestors_to(tp);
    }
    for variant in macos_seatbelt_path_variants(real_home) {
        let line = format!("  (literal \"{}\")", sbpl_escape(&variant));
        if !allow_ancestor_meta_lines.contains(&line) {
            allow_ancestor_meta_lines.push(line);
        }
    }

    let mut allow_write_lines = Vec::new();
    for ws in macos_seatbelt_path_variants(workspace_dir) {
        allow_write_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&ws)));
    }
    for ch in macos_seatbelt_path_variants(cage_home) {
        allow_write_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&ch)));
    }

    let deny_read_block = deny_read_lines.join("\n");
    let allow_cage_read_block = allow_cage_read_lines.join("\n");
    let allow_ancestor_meta_block = allow_ancestor_meta_lines.join("\n");
    let allow_write_block = allow_write_lines.join("\n");

    let mut allow_unix_sock_lines = vec![
        "  (literal \"/private/var/run/syslog\")".to_string(),
        "  (literal \"/var/run/syslog\")".to_string(),
        "  (subpath \"/tmp\")".to_string(),
        "  (subpath \"/private/tmp\")".to_string(),
    ];
    for ch in macos_seatbelt_path_variants(cage_home) {
        allow_unix_sock_lines.push(format!("  (subpath \"{}\")", sbpl_escape(&ch)));
    }
    let allow_unix_sock_block = allow_unix_sock_lines.join("\n");

    format!(
        r#"(version 1)
(allow default)

;; 1. Fail-closed network egress: ONLY allow Berth loopback CageShim & ForwardProxy.
(deny network*)
(allow network-outbound (remote ip "localhost:{shim_port}"))
(allow network-outbound (remote ip "localhost:{proxy_port}"))
(allow network-inbound (local ip "localhost:*"))
;; Restrict outbound Unix domain sockets to syslog/tmp/active cage (blocks mDNSResponder, docker, ssh-agent, etc.).
(allow network-outbound (remote unix-socket
{allow_unix_sock_block}))
(deny network-outbound (remote unix-socket (literal "/private/var/run/mDNSResponder")))
(deny network-outbound (remote unix-socket (literal "/var/run/mDNSResponder")))

;; 2. Hide host timezone, locale, geolocation, system DNS/proxy, ENTIRE real `$HOME` by default,
;;    Berth config/state, and ALL sibling account cages.
;; Note: `/var/db/timezone/zoneinfo` and `/usr/share/zoneinfo` remain readable so `TZ=<IANA>`
;; resolves the Cage timezone cleanly across libc, CoreFoundation (`CFTimeZoneCopySystem`),
;; ICU/Node/Bun, Python, Go, and Rust without ever seeing the host's `/etc/localtime` symlink.
(deny file-read*
  (literal "/etc/localtime")
  (literal "/private/etc/localtime")
  (literal "/var/db/timezone/localtime")
  (literal "/private/var/db/timezone/localtime")
  (literal "/etc/resolv.conf")
  (literal "/private/etc/resolv.conf")
  (literal "/var/run/resolv.conf")
  (literal "/private/var/run/resolv.conf")
  (literal "/Library/Preferences/com.apple.timezone.auto.plist")
  (literal "/Library/Preferences/.GlobalPreferences.plist")
  (literal "/Library/Preferences/.GlobalPreferences_m.plist")
  (subpath "/Library/Preferences/SystemConfiguration")
  (subpath "/private/var/db/timed")
  (subpath "/var/db/timed")
{deny_read_block})
;; Block Mach IPC daemons that bypass file denials to leak real host locale (`en_CN`),
;; host timezone (`timed`), geolocation (`locationd`), and host DNS/VPN proxy (`SystemConfiguration`).
(deny mach-lookup
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.cfprefsd.daemon.system")
  (global-name "com.apple.timed.xpc")
  (global-name-prefix "com.apple.locationd")
  (global-name-prefix "com.apple.CoreLocation")
  (global-name-prefix "com.apple.SystemConfiguration."))
;; Block host process enumeration (`ps aux` sees only self), IOKit hardware serial/UUID
;; (`IOPlatformExpertDevice`), and host-identifying `sysctl` (`hostname`, `uuid`, `routetable`/`ifconfig`).
(deny process-info* (target others))
(deny iokit-open)
(deny iokit-get-properties)
(deny sysctl-read
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.uuid")
  (sysctl-name "kern.boottime")
  (sysctl-name "kern.bootuuid")
  (sysctl-name-prefix "kern.proc")
  (sysctl-name-prefix "net.routetable"))
;; Allow lstat metadata ONLY on exact ancestor literals so realpath()/canonicalize() can reach
;; cage_home/workspace/toolchains without permitting directory listing (`file-read-data`) of real_home!
(allow file-read-metadata
{allow_ancestor_meta_block})
;; Re-allow reading strictly inside the active account's own isolated cage home, workspace, and safe toolchains.
(allow file-read*
{allow_cage_read_block}
{allow_write_block})

;; 3. Restrict file writes strictly to workspace, isolated account home, and temp/tty.
(deny file-write*)
(allow file-write*
{allow_write_block}
  (subpath "/tmp")
  (subpath "/private/tmp")
  (subpath "/private/var/tmp")
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/dtracehelper")
  (literal "/dev/tty")
  (literal "/dev/ptmx")
  (regex #"^/dev/ttys[0-9]+$")
  (regex #"^/dev/fd/[0-9]+$"))
"#
    )
}

#[cfg(target_os = "macos")]
fn launch_macos_seatbelt(
    spec: &SandboxLaunchSpec,
    shim: &CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    if spec.agent == "codex" {
        let _ = write_codex_sandbox_config(
            &spec.account_home,
            &spec.cage,
            &shim.listen_url(),
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
            Some(&spec.workspace_dir),
        );
    }

    let profile = generate_macos_seatbelt_profile_with_bin(
        &spec.workspace_dir,
        &spec.real_home,
        &spec.state_dir,
        &spec.account_home.home_dir,
        shim.listen_addr(),
        shim.proxy_addr(),
        Some(&spec.stock_bin),
    );
    let sb_dir = tempfile::Builder::new()
        .prefix("berth-seatbelt-")
        .tempdir()?;
    let sb_file = sb_dir.path().join("cage.sb");
    write_private_file_atomic(&sb_file, profile.as_bytes())?;

    let mut cmd = Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-f").arg(&sb_file);
    cmd.arg(&spec.stock_bin);
    cmd.args(&spec.agent_args);
    cmd.current_dir(&spec.workspace_dir);

    let cage_tmp = spec.account_home.home_dir.join(".tmp");
    let cage_run = spec.account_home.home_dir.join(".run");
    apply_clean_sandbox_env(&mut cmd, spec, &cage_tmp, &cage_run);
    apply_proxy_and_home_env(
        &mut cmd,
        &spec.account_home,
        &shim.proxy_http_url(),
        &shim.proxy_socks5_url(),
    );

    let child = cmd.spawn()?;
    Ok(wait_child_or_rebind(child, shim, spec.auto_rebind)?)
}

// ---------------------------------------------------------------------------
// Backend 3: Windows Native — AppContainer (`Berth.Cage.Sandbox`) + Zero Network Capabilities
// ---------------------------------------------------------------------------

/// Generates the PowerShell + .NET Win32 AppContainer launcher script (zero `unsafe` in Rust!)
/// that creates/opens `Berth.Cage.Sandbox`, grants ACL ONLY to workspace + isolated cage home,
/// and spawns the target CLI with `CapabilityCount = 0` (NO `internetClient` -> kernel blocks
/// all direct internet TCP/UDP/IPv6/DNS).
pub fn generate_windows_appcontainer_script(
    account_key: &str,
    workspace_dir: &Path,
    cage_home: &Path,
    stock_bin: &Path,
    agent_args: &[String],
) -> String {
    let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let profile_name = windows_appcontainer_profile_name(account_key);
    let ws = ps_quote(&workspace_dir.to_string_lossy());
    let ch = ps_quote(&cage_home.to_string_lossy());
    let bin = ps_quote(&stock_bin.to_string_lossy());
    let mut cmd_parts = Vec::with_capacity(agent_args.len() + 1);
    cmd_parts.push(quote_win32_arg(&stock_bin.to_string_lossy()));
    for a in agent_args {
        cmd_parts.push(quote_win32_arg(a));
    }
    let cmd_line = ps_quote(&cmd_parts.join(" "));

    format!(
        r#"$ErrorActionPreference = 'Stop'
$profileName = '{profile_name}'
$workspace = {ws}
$cageHome = {ch}
$exePath = {bin}
$cmdLine = {cmd_line}

$code = @'
using System;
using System.Runtime.InteropServices;

public static class BerthAppContainer {{
    [StructLayout(LayoutKind.Sequential)]
    public struct SECURITY_CAPABILITIES {{
        public IntPtr AppContainerSid;
        public IntPtr Capabilities;
        public uint CapabilityCount;
        public uint Reserved;
    }}
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO {{
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize;
        public int dwXCountChars; public int dwYCountChars; public int dwFillAttribute;
        public int dwFlags; public short wShowWindow; public short cbReserved2;
        public IntPtr lpReserved2; public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }}
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFOEX {{
        public STARTUPINFO StartupInfo;
        public IntPtr lpAttributeList;
    }}
    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION {{
        public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId;
    }}

    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int CreateAppContainerProfile(string name, string display, string desc, IntPtr caps, uint capCount, out IntPtr sid);
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string name, out IntPtr sid);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSidToStringSid(IntPtr sid, out IntPtr strSid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr lpAttributeList, int dwAttributeCount, int dwFlags, ref IntPtr lpSize);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr lpAttributeList, uint dwFlags, IntPtr Attribute, ref SECURITY_CAPABILITIES lpValue, IntPtr cbSize, IntPtr lpPreviousValue, IntPtr lpReturnSize);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern void DeleteProcThreadAttributeList(IntPtr lpAttributeList);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcess(string lpApplicationName, string lpCommandLine, IntPtr lpProcessAttributes, IntPtr lpThreadAttributes, bool bInheritHandles, uint dwCreationFlags, IntPtr lpEnvironment, string lpCurrentDirectory, ref STARTUPINFOEX lpStartupInfo, out PROCESS_INFORMATION lpProcessInformation);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr hObject);
    [DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr hMem);

    public static string EnsureProfileAndGetSidString(string name) {{
        IntPtr sid;
        int hr = CreateAppContainerProfile(name, "Berth Cage Sandbox", "Zero-leak AI Agent AppContainer", IntPtr.Zero, 0, out sid);
        if (hr != 0) {{
            hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
            if (hr != 0) throw new Exception("DeriveAppContainerSid failed: 0x" + hr.ToString("X"));
        }}
        IntPtr strPtr;
        if (!ConvertSidToStringSid(sid, out strPtr)) throw new Exception("ConvertSidToStringSid failed");
        string s = Marshal.PtrToStringUni(strPtr);
        LocalFree(strPtr);
        return s;
    }}

    public static int RunZeroNet(string name, string exe, string cmdLine, string workDir) {{
        IntPtr sid;
        int hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
        if (hr != 0) throw new Exception("DeriveAppContainerSid failed: 0x" + hr.ToString("X"));

        // CapabilityCount = 0 -> ZERO internetClient / privateNetworkClientServer capabilities!
        var caps = new SECURITY_CAPABILITIES {{ AppContainerSid = sid, Capabilities = IntPtr.Zero, CapabilityCount = 0, Reserved = 0 }};
        IntPtr attrSize = IntPtr.Zero;
        InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref attrSize);
        IntPtr attrBuf = Marshal.AllocHGlobal(attrSize);
        try {{
            if (!InitializeProcThreadAttributeList(attrBuf, 1, 0, ref attrSize)) throw new Exception("InitializeProcThreadAttributeList failed");
            IntPtr PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = (IntPtr)0x00020009;
            if (!UpdateProcThreadAttribute(attrBuf, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, ref caps, (IntPtr)Marshal.SizeOf(caps), IntPtr.Zero, IntPtr.Zero))
                throw new Exception("UpdateProcThreadAttribute failed");
            var si = new STARTUPINFOEX();
            si.StartupInfo.cb = Marshal.SizeOf(si);
            si.lpAttributeList = attrBuf;
            PROCESS_INFORMATION pi;
            uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
            if (!CreateProcess(exe, cmdLine, IntPtr.Zero, IntPtr.Zero, true, EXTENDED_STARTUPINFO_PRESENT, IntPtr.Zero, workDir, ref si, out pi))
                throw new Exception("CreateProcess AppContainer failed: " + Marshal.GetLastWin32Error());
            WaitForSingleObject(pi.hProcess, 0xFFFFFFFF);
            uint exitCode;
            GetExitCodeProcess(pi.hProcess, out exitCode);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return unchecked((int)exitCode);
        }} finally {{
            DeleteProcThreadAttributeList(attrBuf);
            Marshal.FreeHGlobal(attrBuf);
        }}
    }}
}}
'@

Add-Type -TypeDefinition $code
$sid = [BerthAppContainer]::EnsureProfileAndGetSidString($profileName)
& CheckNetIsolation.exe LoopbackExempt -a "-p=$($sid)" | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw "CheckNetIsolation LoopbackExempt failed with exit code $LASTEXITCODE" }}
& icacls $cageHome /grant "*$($sid):(OI)(CI)F" /Q | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw "icacls grant on cageHome failed with exit code $LASTEXITCODE" }}
& icacls $workspace /grant "*$($sid):(OI)(CI)M" /Q | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw "icacls grant on workspace failed with exit code $LASTEXITCODE" }}
try {{
    $rc = [BerthAppContainer]::RunZeroNet($profileName, $exePath, $cmdLine, $workspace)
}} finally {{
    # Revoke workspace ACL grant on exit so prior workspaces do not remain readable across launches.
    & icacls $workspace /remove:g "*$($sid)" /Q 2>$null | Out-Null
}}
exit $rc
"#
    )
}

#[cfg(target_os = "windows")]
fn windows_loopback_exempt_active() -> bool {
    Command::new("CheckNetIsolation.exe")
        .args(["LoopbackExempt", "-s"])
        .output()
        .ok()
        .map(|out| {
            let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
            text.contains(&WINDOWS_APPCONTAINER_NAME.to_ascii_lowercase())
        })
        .unwrap_or(false)
}

/// One-time helper for Windows (`bh sandbox init-windows`): creates the `Berth.Cage.Sandbox`
/// AppContainer profile and grants it loopback-only access to reach `127.0.0.1` CageShim/ForwardProxy.
pub fn run_windows_appcontainer_init() -> Result<(), BerthError> {
    #[cfg(target_os = "windows")]
    {
        let ensure_script = format!(
            r#"
$code = @'
using System;
using System.Runtime.InteropServices;
public static class BerthInit {{
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    public static extern int CreateAppContainerProfile(string name, string display, string desc, IntPtr caps, uint capCount, out IntPtr sid);
}}
'@
Add-Type -TypeDefinition $code
$sid = [IntPtr]::Zero
[void][BerthInit]::CreateAppContainerProfile('{WINDOWS_APPCONTAINER_NAME}', 'Berth Cage Sandbox', 'Zero-leak AI Agent AppContainer', [IntPtr]::Zero, 0, [ref]$sid)
"#
        );
        let _ = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ensure_script])
            .status();

        let status = Command::new("CheckNetIsolation.exe")
            .args([
                "LoopbackExempt",
                "-a",
                &format!("-n={WINDOWS_APPCONTAINER_NAME}"),
            ])
            .status()?;
        if !status.success() {
            return Err(BerthError::Io(
                "CheckNetIsolation LoopbackExempt failed (run this one-time command in an Administrator terminal)".into(),
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(BerthError::InvalidArgument(
            "`bh sandbox init-windows` is only required on native Windows (Linux/WSL2 and macOS require zero setup)".into(),
        ))
    }
}

#[cfg(target_os = "windows")]
fn launch_windows_appcontainer(
    spec: &SandboxLaunchSpec,
    shim: &CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    if spec.agent == "codex" {
        let _ = write_codex_sandbox_config(
            &spec.account_home,
            &spec.cage,
            &shim.listen_url(),
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
            Some(&spec.workspace_dir),
        );
    }
    let script = generate_windows_appcontainer_script(
        &spec.account_home.account_key,
        &spec.workspace_dir,
        &spec.account_home.home_dir,
        &spec.stock_bin,
        &spec.agent_args,
    );
    let tmp = tempfile::Builder::new()
        .prefix("berth-win-ac-")
        .tempdir()?;
    let ps1 = tmp.path().join("launch.ps1");
    write_private_file_atomic(&ps1, script.as_bytes())?;

    let mut cmd = Command::new("powershell.exe");
    scrub_sensitive_host_env(&mut cmd);
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &ps1.to_string_lossy(),
    ]);
    cmd.current_dir(&spec.workspace_dir);
    for key in &spec.overlay_unset_env {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.overlay_set_env {
        cmd.env(key, value);
    }
    spec.launch_env.apply_to_command(&mut cmd);
    apply_proxy_and_home_env(
        &mut cmd,
        &spec.account_home,
        &shim.proxy_http_url(),
        &shim.proxy_socks5_url(),
    );
    let child = cmd.spawn()?;
    Ok(wait_child_or_rebind(child, shim, spec.auto_rebind)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_cage() -> LeaseCageProfile {
        LeaseCageProfile {
            cage_id: "cage-1".into(),
            gate_epoch: 1,
            timezone: "America/Chicago".into(),
            country_code: "US".into(),
            locale: "en_US.UTF-8".into(),
            accept_language: "en-US,en;q=0.9".into(),
            asn_class: "residential_isp".into(),
            strict_mode: true,
        }
    }

    #[test]
    fn account_cage_homes_are_isolated_with_distinct_deterministic_machine_ids() {
        let tmp = TempDir::new().unwrap();
        let cage = sample_cage();
        let h1 = prepare_account_cage_home(tmp.path(), Some(101), &cage).unwrap();
        let h2 = prepare_account_cage_home(tmp.path(), Some(202), &cage).unwrap();
        let h1_again = prepare_account_cage_home(tmp.path(), Some(101), &cage).unwrap();

        assert_ne!(h1.home_dir, h2.home_dir);
        assert_ne!(h1.machine_id, h2.machine_id);
        assert_ne!(h1.hostname, h2.hostname);
        assert_eq!(h1.machine_id.len(), 32);
        assert_eq!(h1.machine_id, h1_again.machine_id);
        assert_eq!(fs::read_to_string(&h1.timezone_file).unwrap().trim(), "America/Chicago");
        assert!(fs::read_to_string(&h1.resolv_conf_file).unwrap().contains("127.0.0.1"));
    }

    #[test]
    fn bwrap_args_enforce_unshare_net_isolated_home_workspace_and_no_wsl_interop() {
        let tmp = TempDir::new().unwrap();
        let cage = sample_cage();
        let acct = prepare_account_cage_home(tmp.path(), Some(42), &cage).unwrap();
        let args = build_bwrap_args(
            Path::new("/work/myproj"),
            Path::new("/home/alice"),
            &acct,
            "America/Chicago",
            Path::new("/tmp/bridge"),
            Path::new("/usr/local/bin/bh"),
            41001,
            41002,
            "claude",
            Path::new("/usr/bin/claude"),
            &["--version".into()],
        );

        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--unshare-uts".to_string()));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"/etc/machine-id".to_string()));
        assert!(args.contains(&"/etc/resolv.conf".to_string()));
        assert!(args.contains(&"WSL_INTEROP".to_string()));
        assert!(args.contains(&"/work/myproj".to_string()));
        assert!(!args.iter().any(|a| a.contains("/run/WSL")));
        assert!(!args.iter().any(|a| a.contains(".ssh")));
    }

    #[test]
    fn macos_seatbelt_profile_blocks_network_mdns_and_ssh_while_allowing_shim_and_workspace() {
        let sb = generate_macos_seatbelt_profile(
            Path::new("/var/folders/ab/work/app"),
            Path::new("/Users/alice"),
            Path::new("/tmp/berth-state"),
            Path::new("/tmp/berth-state/cages/acct-7/home"),
            "127.0.0.1:41001".parse().unwrap(),
            "127.0.0.1:41002".parse().unwrap(),
        );
        assert!(sb.contains("(deny network*)"));
        assert!(sb.contains("(allow network-outbound (remote ip \"localhost:41001\"))"));
        assert!(sb.contains("(allow network-outbound (remote ip \"localhost:41002\"))"));
        assert!(sb.contains("/private/var/run/mDNSResponder"));
        assert!(sb.contains("(literal \"/etc/localtime\")"));
        assert!(sb.contains("(literal \"/private/etc/localtime\")"));
        assert!(sb.contains("(literal \"/etc/resolv.conf\")"));
        assert!(sb.contains("(literal \"/private/var/run/resolv.conf\")"));
        assert!(sb.contains("(literal \"/Library/Preferences/.GlobalPreferences.plist\")"));
        assert!(sb.contains("(subpath \"/Library/Preferences/SystemConfiguration\")"));
        assert!(sb.contains("(global-name \"com.apple.cfprefsd.daemon\")"));
        assert!(sb.contains("(global-name \"com.apple.timed.xpc\")"));
        assert!(sb.contains("(global-name-prefix \"com.apple.SystemConfiguration.\")"));
        assert!(sb.contains("(deny process-info* (target others))"));
        assert!(sb.contains("(deny iokit-open)"));
        assert!(sb.contains("(deny iokit-get-properties)"));
        assert!(sb.contains("(sysctl-name \"kern.hostname\")"));
        assert!(sb.contains("(sysctl-name-prefix \"net.routetable\")"));
        assert!(sb.contains("  (subpath \"/Users/alice\")"));
        assert!(sb.contains("/private/tmp/berth-state/cages"));
        assert!(sb.contains("(allow file-read*\n  (subpath \"/tmp/berth-state/cages/acct-7/home\")"));
        assert!(sb.contains("(subpath \"/var/folders/ab/work/app\")"));
        assert!(sb.contains("(subpath \"/private/var/folders/ab/work/app\")"));
        assert!(sb.contains("(subpath \"/private/tmp/berth-state/cages/acct-7/home\")"));
    }

    #[test]
    fn windows_appcontainer_script_uses_zero_network_capabilities_and_per_account_sid() {
        let ps = generate_windows_appcontainer_script(
            "acct-7",
            Path::new("C:\\work\\repo"),
            Path::new("C:\\Users\\alice\\AppData\\Local\\berth\\cages\\acct-7\\home"),
            Path::new("C:\\Tools\\claude.exe"),
            &["--prompt".into(), "say \"hi\" \\".into()],
        );
        assert!(ps.contains("Berth.Cage.Sandbox.acct-7"));
        assert!(ps.contains("CapabilityCount = 0"));
        assert!(!ps.contains("internetClient\""));
        assert!(ps.contains("C:\\work\\repo"));
        assert_eq!(quote_win32_arg("a\\b\\"), "a\\b\\");
        assert_eq!(quote_win32_arg("say \"hi\" \\"), "\"say \\\"hi\\\" \\\\\"");
    }

    #[test]
    fn validate_sandbox_workspace_rejects_home_and_state_overlap() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = home.join(".local/state/berth");
        let project = home.join("code/proj");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&project).unwrap();

        assert!(validate_sandbox_workspace(&project, &home, &state).is_ok());
        assert!(validate_sandbox_workspace(&home, &home, &state).is_err());
        assert!(validate_sandbox_workspace(tmp.path(), &home, &state).is_err());
        assert!(validate_sandbox_workspace(&state, &home, &state).is_err());
        assert!(validate_sandbox_workspace(&state.join("cages/acct-1"), &home, &state).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_live_sandbox_blocks_direct_egress_hides_ssh_and_allows_workspace_rw() {
        let probe = probe_sandbox_backend();
        if !probe.available {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("workspace");
        let fake_real_home = tmp.path().join("real_home");
        let bridge = tmp.path().join("bridge");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(fake_real_home.join(".ssh")).unwrap();
        fs::create_dir_all(&bridge).unwrap();
        fs::write(fake_real_home.join(".ssh").join("id_ed25519"), "SECRET_KEY").unwrap();

        let cage = sample_cage();
        let acct = prepare_account_cage_home(tmp.path(), Some(77), &cage).unwrap();
        let bwrap = which::which("bwrap").unwrap();

        // Direct bwrap test verifying:
        // 1. ~/.ssh is completely hidden (replaced by isolated cage home)
        // 2. /etc/machine-id equals deterministic cage machine_id
        // 3. workspace is read-write
        // 4. direct outbound socket to 1.1.1.1:443 fails immediately (ENETUNREACH) inside --unshare-net
        let script = format!(
            "set -e; \
             [ ! -e '{home}/.ssh/id_ed25519' ]; \
             mid=$(cat /etc/machine-id); [ \"$mid\" = '{mid}' ]; \
             echo ok > '{ws}/written.txt'; \
             if python3 -c \"import socket; s=socket.socket(); s.settimeout(1); s.connect(('1.1.1.1', 443))\" 2>/dev/null; then exit 99; fi",
            home = fake_real_home.display(),
            mid = acct.machine_id,
            ws = ws.display(),
        );
        let mut args = vec![
            "--unshare-net".to_string(),
            "--unshare-uts".to_string(),
            "--unshare-pid".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
        ];
        for top in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt"] {
            let p = Path::new(top);
            if let Ok(meta) = fs::symlink_metadata(p) {
                if meta.file_type().is_symlink() {
                    if let Ok(t) = fs::read_link(p) {
                        args.extend(["--symlink".into(), t.to_string_lossy().into_owned(), top.into()]);
                    }
                } else if meta.is_dir() {
                    args.extend(["--ro-bind".into(), top.into(), top.into()]);
                }
            }
        }
        args.extend([
            "--bind".into(),
            acct.home_dir.to_string_lossy().into_owned(),
            fake_real_home.to_string_lossy().into_owned(),
            "--bind".into(),
            ws.to_string_lossy().into_owned(),
            ws.to_string_lossy().into_owned(),
            "--ro-bind".into(),
            acct.machine_id_file.to_string_lossy().into_owned(),
            "/etc/machine-id".into(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            script,
        ]);
        let st = Command::new(bwrap).args(&args).status().unwrap();
        assert!(st.success(), "bwrap isolation verification failed: {st}");
        assert_eq!(fs::read_to_string(ws.join("written.txt")).unwrap().trim(), "ok");
    }
}
