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
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::thread;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::berth::{
    BerthError, CliLaunchEnv, LeaseCageProfile, write_codex_overlay_config_toml,
    write_private_file_atomic,
};
use crate::cage_shim::CageShimHandle;

pub const WINDOWS_APPCONTAINER_NAME: &str = "Berth.Cage.Sandbox";
const TRAMPOLINE_SHIM_TOKEN_ENV: &str = "BERTH_TRAMPOLINE_SHIM_TOKEN";

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
    pub account_home: AccountCageHome,
    pub cage: LeaseCageProfile,
    pub stock_bin: PathBuf,
    pub agent_args: Vec<String>,
    pub launch_env: CliLaunchEnv,
    pub overlay_unset_env: Vec<String>,
    pub overlay_set_env: BTreeMap<String, String>,
    pub overlay_fs_etc: Option<PathBuf>,
    pub allow_env_fallback: bool,
}

/// Probes native zero-VM sandbox capabilities on Linux/WSL2, macOS, or Windows.
pub fn probe_sandbox_backend() -> SandboxProbe {
    let os = std::env::consts::OS.to_string();
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
    let root_dir = state_dir.join("cages").join(&account_key);
    let home_dir = root_dir.join("home");
    let etc_dir = root_dir.join("etc");

    for sub in [
        &home_dir,
        &etc_dir,
        &home_dir.join(".config"),
        &home_dir.join(".cache"),
        &home_dir.join(".local").join("share"),
        &home_dir.join(".local").join("state"),
        &home_dir.join(".claude"),
        &home_dir.join(".codex"),
    ] {
        fs::create_dir_all(sub)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&root_dir, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&home_dir, fs::Permissions::from_mode(0o700));
    }

    let machine_id = deterministic_hex(&format!("berth-cage-machine-id:{account_key}"), 32);
    let hostname = format!(
        "berth-{}",
        deterministic_hex(&format!("berth-cage-host:{account_key}"), 8)
    );

    let machine_id_file = etc_dir.join("machine-id");
    let timezone_file = etc_dir.join("timezone");
    let resolv_conf_file = etc_dir.join("resolv.conf");
    let hostname_file = etc_dir.join("hostname");
    let hosts_file = etc_dir.join("hosts");

    write_private_file_atomic(&machine_id_file, format!("{machine_id}\n").as_bytes())?;
    write_private_file_atomic(&timezone_file, format!("{}\n", cage.timezone).as_bytes())?;
    write_private_file_atomic(
        &resolv_conf_file,
        b"# Managed by Berth Sandbox (DNS resolved remotely on Landing Node via proxy)\nnameserver 127.0.0.1\n",
    )?;
    write_private_file_atomic(&hostname_file, format!("{hostname}\n").as_bytes())?;
    write_private_file_atomic(
        &hosts_file,
        format!("127.0.0.1 localhost {hostname}\n").as_bytes(),
    )?;

    Ok(AccountCageHome {
        account_key,
        root_dir,
        home_dir,
        etc_dir,
        machine_id_file,
        timezone_file,
        resolv_conf_file,
        hostname_file,
        hosts_file,
        hostname,
        machine_id,
    })
}

/// Writes Codex `[shell_environment_policy]` into the isolated per-account `$HOME/.codex/config.toml`
/// preserving TZ/locale AND the Berth proxy env vars for Codex subshells & MCP tools.
pub fn write_codex_sandbox_config(
    account_home: &AccountCageHome,
    cage: &LeaseCageProfile,
    http_proxy_url: &str,
    socks5_proxy_url: &str,
) -> Result<PathBuf, BerthError> {
    let quote = |raw: &str| -> String {
        let esc = raw
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("\"{esc}\"")
    };
    let snippet = format!(
        "[shell_environment_policy]\ninherit = \"core\"\n\n[shell_environment_policy.set]\n\
         TZ = {}\nLANG = {}\nLC_ALL = {}\nLC_TIME = {}\n\
         HTTP_PROXY = {}\nHTTPS_PROXY = {}\nALL_PROXY = {}\n\
         http_proxy = {}\nhttps_proxy = {}\nall_proxy = {}\n\
         NO_PROXY = \"127.0.0.1,localhost\"\nno_proxy = \"127.0.0.1,localhost\"\n",
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
    );
    write_codex_overlay_config_toml(&account_home.home_dir.join(".codex"), &snippet)
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
    cmd.env("BERTH_SANDBOX_ACCOUNT", &account_home.account_key);
}

/// Launches the stock CLI inside the OS-native zero-VM sandbox (Linux/WSL2 `bwrap`,
/// macOS `sandbox-exec`, or Windows `AppContainer`), routing ALL outbound network traffic
/// through `shim`'s `CageShim` + `ForwardProxy` -> Berth Landing Node.
pub fn launch_in_sandbox(
    spec: &SandboxLaunchSpec,
    shim: &mut CageShimHandle,
) -> Result<ExitStatus, BerthError> {
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
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
        );
    }
    let mut cmd = Command::new(&spec.stock_bin);
    cmd.args(&spec.agent_args);
    cmd.current_dir(&spec.workspace_dir);
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
    Ok(cmd.status()?)
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
                    "localtime" | "timezone" | "machine-id" | "resolv.conf" | "hostname" | "hosts"
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
    for safe_tool_rel in [
        ".cargo/bin",
        ".rustup",
        ".local/bin",
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

    // If stock_bin lives in an uncommon non-system path (e.g. custom prefix), expose its parent dir read-only
    // unless it sits inside real_home sensitive dirs.
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
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
        );
    }

    let mut cmd = Command::new(&bwrap_bin);
    cmd.args(&bwrap_args);
    for key in &spec.overlay_unset_env {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.overlay_set_env {
        cmd.env(key, value);
    }
    spec.launch_env.apply_to_command(&mut cmd);
    // Inside bwrap, real_home path IS bind-mounted to account_home.home_dir!
    cmd.env("HOME", &spec.real_home);
    cmd.env("XDG_CONFIG_HOME", spec.real_home.join(".config"));
    cmd.env("XDG_CACHE_HOME", spec.real_home.join(".cache"));
    cmd.env("XDG_DATA_HOME", spec.real_home.join(".local").join("share"));
    cmd.env("XDG_STATE_HOME", spec.real_home.join(".local").join("state"));
    cmd.env("CODEX_HOME", spec.real_home.join(".codex"));
    cmd.env("CLAUDE_CONFIG_DIR", spec.real_home.join(".claude"));
    cmd.env(TRAMPOLINE_SHIM_TOKEN_ENV, shim.shim_token());
    cmd.env("BERTH_SANDBOX_BACKEND", "linux-bwrap");
    cmd.env("BERTH_SANDBOX_ACCOUNT", &spec.account_home.account_key);

    let status = cmd.status();
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

/// Generates a fail-closed macOS Seatbelt (`.sb`) profile enforcing:
/// - `(deny network*)`, allowing ONLY `localhost:<shim_port>` and `localhost:<proxy_port>`
/// - Denies `/private/var/run/mDNSResponder` (blocks host DNS lookups)
/// - Hides `~/.ssh`, `~/.berth`, `~/.gnupg`, `~/.aws`, `~/.kube`
/// - Allows writes ONLY to `$PWD`, `<cage_home>`, and temp/tty devices.
pub fn generate_macos_seatbelt_profile(
    workspace_dir: &Path,
    real_home: &Path,
    cage_home: &Path,
    shim_addr: SocketAddr,
    proxy_addr: SocketAddr,
) -> String {
    let ws = sbpl_escape(workspace_dir);
    let ch = sbpl_escape(cage_home);
    let rh = sbpl_escape(real_home);
    let shim_port = shim_addr.port();
    let proxy_port = proxy_addr.port();

    format!(
        r#"(version 1)
(allow default)

;; 1. Fail-closed network egress: ONLY allow Berth loopback CageShim & ForwardProxy.
(deny network*)
(allow network-outbound (remote ip "localhost:{shim_port}"))
(allow network-outbound (remote ip "localhost:{proxy_port}"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote unix-socket))
(deny network-outbound (remote unix-socket (literal "/private/var/run/mDNSResponder")))

;; 2. Hide host sensitive credential directories.
(deny file-read*
  (subpath "{rh}/.ssh")
  (subpath "{rh}/.berth")
  (subpath "{rh}/.gnupg")
  (subpath "{rh}/.aws")
  (subpath "{rh}/.kube"))

;; 3. Restrict file writes strictly to workspace, isolated account home, and temp/tty.
(deny file-write*)
(allow file-write*
  (subpath "{ws}")
  (subpath "{ch}")
  (subpath "/tmp")
  (subpath "/private/tmp")
  (subpath "/private/var/tmp")
  (subpath "/private/var/folders")
  (literal "/dev/null")
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
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
        );
    }

    let profile = generate_macos_seatbelt_profile(
        &spec.workspace_dir,
        &spec.real_home,
        &spec.account_home.home_dir,
        shim.listen_addr(),
        shim.proxy_addr(),
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
    cmd.env("BERTH_SANDBOX_BACKEND", "macos-seatbelt");

    Ok(cmd.status()?)
}

// ---------------------------------------------------------------------------
// Backend 3: Windows Native — AppContainer (`Berth.Cage.Sandbox`) + Zero Network Capabilities
// ---------------------------------------------------------------------------

/// Generates the PowerShell + .NET Win32 AppContainer launcher script (zero `unsafe` in Rust!)
/// that creates/opens `Berth.Cage.Sandbox`, grants ACL ONLY to workspace + isolated cage home,
/// and spawns the target CLI with `CapabilityCount = 0` (NO `internetClient` -> kernel blocks
/// all direct internet TCP/UDP/IPv6/DNS).
pub fn generate_windows_appcontainer_script(
    workspace_dir: &Path,
    cage_home: &Path,
    stock_bin: &Path,
    agent_args: &[String],
) -> String {
    let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let ws = ps_quote(&workspace_dir.to_string_lossy());
    let ch = ps_quote(&cage_home.to_string_lossy());
    let bin = ps_quote(&stock_bin.to_string_lossy());
    let args_joined = agent_args
        .iter()
        .map(|a| {
            let escaped = a.replace('"', "\\\"");
            if escaped.contains(' ') || escaped.is_empty() {
                format!("\"{escaped}\"")
            } else {
                escaped
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let cmd_line = ps_quote(&format!("\"{}\" {}", stock_bin.to_string_lossy(), args_joined));

    format!(
        r#"$ErrorActionPreference = 'Stop'
$profileName = '{WINDOWS_APPCONTAINER_NAME}'
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
& icacls $workspace /grant "*$($sid):(OI)(CI)M" /Q | Out-Null
& icacls $cageHome /grant "*$($sid):(OI)(CI)F" /Q | Out-Null
$rc = [BerthAppContainer]::RunZeroNet($profileName, $exePath, $cmdLine, $workspace)
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
            &shim.proxy_http_url(),
            &shim.proxy_socks5_url(),
        );
    }
    let script = generate_windows_appcontainer_script(
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
    cmd.env("BERTH_SANDBOX_BACKEND", "windows-appcontainer");
    Ok(cmd.status()?)
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
            Path::new("/Users/alice/code/app"),
            Path::new("/Users/alice"),
            Path::new("/Users/alice/.local/state/berth/cages/acct-7/home"),
            "127.0.0.1:41001".parse().unwrap(),
            "127.0.0.1:41002".parse().unwrap(),
        );
        assert!(sb.contains("(deny network*)"));
        assert!(sb.contains("(allow network-outbound (remote ip \"localhost:41001\"))"));
        assert!(sb.contains("(allow network-outbound (remote ip \"localhost:41002\"))"));
        assert!(sb.contains("/private/var/run/mDNSResponder"));
        assert!(sb.contains("/Users/alice/.ssh"));
        assert!(sb.contains("/Users/alice/.berth"));
        assert!(sb.contains("(subpath \"/Users/alice/code/app\")"));
    }

    #[test]
    fn windows_appcontainer_script_uses_zero_network_capabilities() {
        let ps = generate_windows_appcontainer_script(
            Path::new("C:\\work\\repo"),
            Path::new("C:\\Users\\alice\\AppData\\Local\\berth\\cages\\acct-7\\home"),
            Path::new("C:\\Tools\\claude.exe"),
            &["--help".into()],
        );
        assert!(ps.contains(WINDOWS_APPCONTAINER_NAME));
        assert!(ps.contains("CapabilityCount = 0"));
        assert!(!ps.contains("internetClient\""));
        assert!(ps.contains("C:\\work\\repo"));
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
