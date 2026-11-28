//! Optional Apple Silicon (`aarch64-apple-darwin`) ARM64 Linux Micro-VM sandbox backend
//! powered directly by Apple's native `Virtualization.framework` (`VZVirtualMachine`).
//!
//! Security & isolation guarantees (beyond macOS Seatbelt):
//! 1. **Hardware-enforced zero network device**: runtime VMs attach ZERO `virtio-net` NICs
//!    (only `lo` exists inside the Linux guest). All AI & proxy traffic traverses `virtio-vsock`
//!    (`AF_VSOCK` CID=2 ports 5001/5002) strictly into host-side `CageShim` and `ForwardProxy`.
//! 2. **Full Linux kernel / mount / UTS / PID / user isolation**:
//!    - `uname` reports a genuine ARM64 Linux kernel and deterministic Cage hostname (`dev-xxxxxx`).
//!    - Current workspace is mounted via VirtioFS strictly at `/workspace` (host `/Users/<name>`
//!      paths never exist inside the VM).
//!    - Isolated per-account Cage home is mounted at `/home/dev` with synthetic non-root user `dev`
//!      mapped to the host UID/GID so file ownership on VirtioFS stays clean without leaking the
//!      macOS username.
//!    - `/etc` runs on a per-boot `tmpfs` `overlayfs` over a **read-only** `rootfs.raw` disk image
//!      (safe for concurrent `bh claude` / `bh codex` / `bh agy` sessions), with authoritative
//!      `/etc/localtime`, `/etc/timezone`, `/etc/locale.conf`, `/etc/machine-id`, `/etc/passwd`,
//!      and `/etc/resolv.conf`.
//! 3. **Real execution OS alignment**: genuine Linux `aarch64` CLIs run inside the VM, matching
//!    `X-Berth-Exec-OS: Linux` + `X-Berth-Exec-Arch: arm64` sent by `CageShim` to Berth Edge.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::process::Command;
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use sha2::{Digest, Sha256};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::berth::write_private_file_atomic;
use crate::berth::BerthError;
use crate::cage_shim::CageShimHandle;
use crate::sandbox::SandboxLaunchSpec;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::sandbox::{
    validate_sandbox_workspace, wait_child_or_rebind, write_claude_sandbox_config,
    write_codex_sandbox_config,
};

pub const VM_DIR_NAME: &str = "vm";
pub const VM_HELPER_NAME: &str = "berth-vz";
pub const VM_HELPER_HASH_NAME: &str = "berth-vz.sha256";
pub const VM_KERNEL_NAME: &str = "vmlinux";
pub const VM_INITRD_NAME: &str = "initrd.img";
pub const VM_ROOTFS_NAME: &str = "rootfs.raw";
pub const VM_META_NAME: &str = "guest_os.json";

pub const DEBIAN_NOCLOUD_ARM64_URL: &str =
    "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-nocloud-arm64.tar.xz";
pub const BOOTSTRAP_KERNEL_URL: &str =
    "https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-vmlinuz-generic";
pub const BOOTSTRAP_INITRD_URL: &str =
    "https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-initrd-generic";

pub const VSOCK_SHIM_PORT: u32 = 5001;
pub const VSOCK_PROXY_PORT: u32 = 5002;
pub const VSOCK_PTY_PORT: u32 = 5003;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacosVmMeta {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub root_part: String,
}

impl Default for MacosVmMeta {
    fn default() -> Self {
        Self {
            os: "Linux".into(),
            os_version: "Debian 13".into(),
            arch: "arm64".into(),
            root_part: "/dev/vda1".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacosVmStatus {
    pub supported: bool,
    pub initialized: bool,
    pub helper_ready: bool,
    pub kernel_ready: bool,
    pub rootfs_ready: bool,
    pub vm_dir: PathBuf,
    pub guest_os: Option<MacosVmMeta>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmLaunchConfig {
    pub agent: String,
    pub command: Vec<String>,
    pub hostname: String,
    pub machine_id: String,
    pub timezone: String,
    pub locale: String,
    pub uid: u32,
    pub gid: u32,
    pub shim_port: u16,
    pub proxy_port: u16,
    pub vsock_shim_port: u32,
    pub vsock_proxy_port: u32,
    pub vsock_pty_port: u32,
    pub env: BTreeMap<String, String>,
}

pub fn vm_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(VM_DIR_NAME)
}

pub fn read_vm_guest_meta(state_dir: &Path) -> MacosVmMeta {
    let meta_path = vm_dir_for_state(state_dir).join(VM_META_NAME);
    fs::read_to_string(meta_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<MacosVmMeta>(&raw).ok())
        .unwrap_or_default()
}

pub fn is_apple_silicon_macos() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// Probes Apple Silicon ARM64 Linux Micro-VM readiness on the current host.
pub fn probe_macos_vm(state_dir: &Path) -> MacosVmStatus {
    let vm_dir = vm_dir_for_state(state_dir);
    let supported = is_apple_silicon_macos();
    let helper_ready = vm_dir.join(VM_HELPER_NAME).is_file();
    let kernel_ready = vm_dir.join(VM_KERNEL_NAME).is_file() && vm_dir.join(VM_INITRD_NAME).is_file();
    let rootfs_ready = vm_dir.join(VM_ROOTFS_NAME).is_file();
    let initialized = supported && helper_ready && kernel_ready && rootfs_ready;
    let guest_os = if initialized {
        Some(read_vm_guest_meta(state_dir))
    } else {
        None
    };

    let mut notes = Vec::new();
    if !supported {
        notes.push("macOS ARM64 Micro-VM backend is supported exclusively on Apple Silicon macOS (aarch64-apple-darwin)".into());
    } else if initialized {
        notes.push(format!(
            "Apple Silicon ARM64 Linux Micro-VM ready at {} (zero virtio-net NIC, VirtioFS /workspace, virtio-vsock CageShim/ForwardProxy)",
            vm_dir.display()
        ));
    } else {
        notes.push(
            "Apple Silicon ARM64 Linux Micro-VM not initialized yet; run once: `bh sandbox init-vm`"
                .into(),
        );
    }

    MacosVmStatus {
        supported,
        initialized,
        helper_ready,
        kernel_ready,
        rootfs_ready,
        vm_dir,
        guest_os,
        notes,
    }
}

/// Builds the per-launch JSON payload consumed by `vm_init.py` inside the ARM64 Linux guest.
/// Public so unit tests on Linux/macOS/Windows verify zero host secret/path leakage deterministically.
pub fn build_vm_launch_config(
    spec: &SandboxLaunchSpec,
    shim_port: u16,
    proxy_port: u16,
    shim_token: &str,
    uid: u32,
    gid: u32,
) -> VmLaunchConfig {
    let mut env = BTreeMap::new();
    for key in ["TERM", "COLORTERM", "NO_COLOR", "FORCE_COLOR", "COLUMNS", "LINES"] {
        if let Ok(val) = std::env::var(key) {
            if !val.trim().is_empty() {
                env.insert(key.to_string(), val);
            }
        }
    }
    env.entry("TERM".into())
        .or_insert_with(|| "xterm-256color".into());

    for (k, v) in &spec.overlay_set_env {
        if !k.starts_with("AGS_BERTH_") && !k.starts_with("BERTH_") {
            env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &spec.launch_env.set_env {
        if !k.starts_with("AGS_BERTH_") && !k.starts_with("BERTH_") {
            env.insert(k.clone(), v.clone());
        }
    }
    for k in &spec.overlay_unset_env {
        env.remove(k);
    }
    for k in &spec.launch_env.unset_env {
        env.remove(k);
    }

    let base_url = format!("http://127.0.0.1:{shim_port}");
    let http_proxy = format!("http://bh:{shim_token}@127.0.0.1:{proxy_port}");
    let socks5_proxy = format!("socks5h://bh:{shim_token}@127.0.0.1:{proxy_port}");

    match spec.agent.as_str() {
        "codex" => {
            env.insert("OPENAI_BASE_URL".into(), format!("{base_url}/v1"));
            env.insert("OPENAI_API_KEY".into(), shim_token.to_string());
        }
        "claude" => {
            env.insert("ANTHROPIC_BASE_URL".into(), base_url.clone());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), shim_token.to_string());
            env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into());
            env.insert("DISABLE_TELEMETRY".into(), "1".into());
        }
        "agy" => {
            env.insert("AGY_LLM_GATEWAY_URL".into(), base_url.clone());
            env.insert("AGY_LLM_GATEWAY_API_KEY".into(), shim_token.to_string());
            env.insert("GEMINI_API_BASE".into(), base_url.clone());
            env.insert("GOOGLE_GEMINI_BASE_URL".into(), base_url.clone());
            env.insert("GEMINI_API_KEY".into(), shim_token.to_string());
        }
        "grok" => {
            env.insert("XAI_BASE_URL".into(), format!("{base_url}/v1"));
            env.insert("GROK_BASE_URL".into(), format!("{base_url}/v1"));
            env.insert("XAI_API_KEY".into(), shim_token.to_string());
            env.insert("GROK_API_KEY".into(), shim_token.to_string());
        }
        _ => {}
    }

    // Authoritative guest Linux Cage identity & proxy settings.
    env.insert("HOME".into(), "/home/dev".into());
    env.insert("USER".into(), "dev".into());
    env.insert("LOGNAME".into(), "dev".into());
    env.insert("SHELL".into(), "/bin/bash".into());
    env.insert("HOSTNAME".into(), spec.account_home.hostname.clone());
    env.insert("TMPDIR".into(), "/tmp".into());
    env.insert("XDG_RUNTIME_DIR".into(), "/tmp/runtime".into());
    env.insert("XDG_CONFIG_HOME".into(), "/home/dev/.config".into());
    env.insert("XDG_CACHE_HOME".into(), "/home/dev/.cache".into());
    env.insert("XDG_DATA_HOME".into(), "/home/dev/.local/share".into());
    env.insert("XDG_STATE_HOME".into(), "/home/dev/.local/state".into());
    env.insert("CODEX_HOME".into(), "/home/dev/.codex".into());
    env.insert("CLAUDE_CONFIG_DIR".into(), "/home/dev/.claude".into());
    env.insert(
        "PATH".into(),
        "/home/dev/.local/bin:/usr/local/bin:/usr/bin:/bin".into(),
    );
    env.insert("TZ".into(), spec.cage.timezone.clone());
    env.insert("LANG".into(), spec.cage.locale.clone());
    env.insert("LC_ALL".into(), spec.cage.locale.clone());
    env.insert("LC_TIME".into(), spec.cage.locale.clone());
    env.insert("HTTP_PROXY".into(), http_proxy.clone());
    env.insert("HTTPS_PROXY".into(), http_proxy.clone());
    env.insert("ALL_PROXY".into(), socks5_proxy.clone());
    env.insert("http_proxy".into(), http_proxy);
    env.insert("https_proxy".into(), env["HTTPS_PROXY"].clone());
    env.insert("all_proxy".into(), socks5_proxy);
    env.insert("NO_PROXY".into(), "127.0.0.1,localhost".into());
    env.insert("no_proxy".into(), "127.0.0.1,localhost".into());

    let mut command = Vec::with_capacity(spec.agent_args.len() + 1);
    let guest_bin = match spec.agent.as_str() {
        "claude" | "codex" | "agy" | "grok" => spec.agent.clone(),
        _ => spec.stock_bin.to_string_lossy().into_owned(),
    };
    command.push(guest_bin);
    command.extend(spec.agent_args.iter().cloned());

    VmLaunchConfig {
        agent: spec.agent.clone(),
        command,
        hostname: spec.account_home.hostname.clone(),
        machine_id: spec.account_home.machine_id.clone(),
        timezone: spec.cage.timezone.clone(),
        locale: spec.cage.locale.clone(),
        uid,
        gid,
        shim_port,
        proxy_port,
        vsock_shim_port: VSOCK_SHIM_PORT,
        vsock_proxy_port: VSOCK_PROXY_PORT,
        vsock_pty_port: VSOCK_PTY_PORT,
        env,
    }
}

/// Builds the argv for `berth-vz` in runtime zero-network Micro-VM mode.
pub fn build_vz_runtime_args(
    vm_dir: &Path,
    meta: &MacosVmMeta,
    workspace_dir: &Path,
    cage_home_dir: &Path,
    etc_dir: &Path,
    shim_port: u16,
    proxy_port: u16,
    console_log: &Path,
) -> Vec<String> {
    let root_part = if meta.root_part.trim().is_empty() {
        "/dev/vda1"
    } else {
        meta.root_part.trim()
    };
    let cmdline = format!(
        "console=hvc0 root={root_part} ro rootflags=noload fsck.mode=skip quiet loglevel=3 init=/usr/local/bin/berth-vm-init"
    );
    vec![
        "--kernel".into(),
        vm_dir.join(VM_KERNEL_NAME).to_string_lossy().into_owned(),
        "--initrd".into(),
        vm_dir.join(VM_INITRD_NAME).to_string_lossy().into_owned(),
        "--cmdline".into(),
        cmdline,
        "--disk".into(),
        vm_dir.join(VM_ROOTFS_NAME).to_string_lossy().into_owned(),
        "--disk-ro".into(),
        "--share".into(),
        format!("workspace={}", workspace_dir.display()),
        "--share".into(),
        format!("cagehome={}", cage_home_dir.display()),
        "--share".into(),
        format!("cageetc={}:ro", etc_dir.display()),
        "--vsock-forward".into(),
        format!("{VSOCK_SHIM_PORT}=127.0.0.1:{shim_port}"),
        "--vsock-forward".into(),
        format!("{VSOCK_PROXY_PORT}=127.0.0.1:{proxy_port}"),
        "--vsock-pty-port".into(),
        VSOCK_PTY_PORT.to_string(),
        "--console-log".into(),
        console_log.to_string_lossy().into_owned(),
    ]
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn helper_source_hash() -> String {
    let mut h = Sha256::new();
    h.update(BERTH_VZ_SWIFT_SRC.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Compiles & ad-hoc signs the native Apple `Virtualization.framework` helper (`berth-vz`)
/// if missing or stale.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn ensure_vz_helper(vm_dir: &Path) -> Result<PathBuf, BerthError> {
    fs::create_dir_all(vm_dir)?;
    let helper_bin = vm_dir.join(VM_HELPER_NAME);
    let hash_file = vm_dir.join(VM_HELPER_HASH_NAME);
    let want_hash = helper_source_hash();
    if helper_bin.is_file() {
        if let Ok(cur) = fs::read_to_string(&hash_file) {
            if cur.trim() == want_hash {
                return Ok(helper_bin);
            }
        }
    }

    let tmp = tempfile::Builder::new().prefix("berth-vz-build-").tempdir()?;
    let swift_file = tmp.path().join("main.swift");
    let ent_file = tmp.path().join("entitlements.plist");
    let out_bin = tmp.path().join(VM_HELPER_NAME);
    fs::write(&swift_file, BERTH_VZ_SWIFT_SRC)?;
    fs::write(&ent_file, VZ_ENTITLEMENTS_PLIST)?;

    let swiftc = if Path::new("/usr/bin/swiftc").is_file() {
        PathBuf::from("/usr/bin/swiftc")
    } else {
        which::which("swiftc").map_err(|_| {
            BerthError::StrictIsolationUnavailable(
                "`swiftc` not found; install Apple Command Line Tools (`xcode-select --install`) to build the Apple Silicon Virtualization.framework helper".into(),
            )
        })?
    };

    let build = Command::new(&swiftc)
        .args(["-swift-version", "5", "-O"])
        .arg(&swift_file)
        .arg("-o")
        .arg(&out_bin)
        .output()?;
    if !build.status.success() {
        return Err(BerthError::Io(format!(
            "failed to compile Apple Silicon `berth-vz` helper: {}",
            String::from_utf8_lossy(&build.stderr).trim()
        )));
    }

    let sign = Command::new("/usr/bin/codesign")
        .args(["--entitlements"])
        .arg(&ent_file)
        .args(["--force", "-s", "-"])
        .arg(&out_bin)
        .output()?;
    if !sign.status.success() {
        return Err(BerthError::Io(format!(
            "failed to sign `berth-vz` with com.apple.security.virtualization: {}",
            String::from_utf8_lossy(&sign.stderr).trim()
        )));
    }

    // Atomic install into vm_dir.
    let staged = vm_dir.join(format!("{VM_HELPER_NAME}.tmp-{}", std::process::id()));
    fs::copy(&out_bin, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&staged, fs::Permissions::from_mode(0o755));
    }
    fs::rename(&staged, &helper_bin)?;
    write_private_file_atomic(&hash_file, format!("{want_hash}\n").as_bytes())?;
    Ok(helper_bin)
}

/// Minimal authenticated HTTP/1.1 setup server used strictly during one-time `bh sandbox init-vm`
/// so the Debian 13 ARM64 setup guest can pull `setup.sh`/`berth-vm-init` and upload its matched
/// `vmlinux` + `initrd.img` over the Apple `VZNATNetworkDeviceAttachment` host gateway (`192.168.64.1`).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn start_vm_setup_http_server(
    setup_dir: PathBuf,
    setup_script: String,
    token: String,
) -> Result<(u16, std::sync::Arc<std::sync::atomic::AtomicBool>), BerthError> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;

    let listener = TcpListener::bind("0.0.0.0:0")?;
    let port = listener.local_addr()?.port();
    let _ = listener.set_nonblocking(true);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);

    thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(60)));
                    let dir = setup_dir.clone();
                    let script = setup_script.clone();
                    let tok = token.clone();
                    thread::spawn(move || {
                        let mut br = BufReader::new(&mut stream);
                        let mut req_line = String::new();
                        if br.read_line(&mut req_line).is_err() || req_line.is_empty() {
                            return;
                        }
                        let mut parts = req_line.split_whitespace();
                        let method = parts.next().unwrap_or("").to_string();
                        let path = parts.next().unwrap_or("").to_string();
                        let mut content_len: usize = 0;
                        let mut auth_ok = false;
                        loop {
                            let mut h = String::new();
                            if br.read_line(&mut h).is_err() || h == "\r\n" || h == "\n" || h.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = h.split_once(':') {
                                if k.eq_ignore_ascii_case("content-length") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                } else if k.eq_ignore_ascii_case("x-setup-token") {
                                    auth_ok = v.trim() == tok;
                                }
                            }
                        }
                        if !auth_ok {
                            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            return;
                        }
                        match (method.as_str(), path.as_str()) {
                            ("GET", "/setup.sh") => {
                                let b = script.as_bytes();
                                let hdr = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                                let _ = stream.write_all(hdr.as_bytes());
                                let _ = stream.write_all(b);
                            }
                            ("GET", "/berth-vm-init") => {
                                let b = GUEST_VM_INIT_SH.as_bytes();
                                let hdr = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                                let _ = stream.write_all(hdr.as_bytes());
                                let _ = stream.write_all(b);
                            }
                            ("PUT", p) if p.starts_with("/upload/") => {
                                let name = &p["/upload/".len()..];
                                let allowed = matches!(name, "vmlinux" | "initrd.img" | "ROOT_PART" | "SETUP_OK");
                                if !allowed || content_len > 256 * 1024 * 1024 {
                                    let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                                    return;
                                }
                                let dst = dir.join(name);
                                if let Ok(mut f) = fs::File::create(&dst) {
                                    let mut rem = content_len;
                                    let mut buf = [0u8; 64 * 1024];
                                    let mut ok = true;
                                    while rem > 0 {
                                        let want = rem.min(buf.len());
                                        match br.read(&mut buf[..want]) {
                                            Ok(0) | Err(_) => { ok = false; break; }
                                            Ok(n) => {
                                                if f.write_all(&buf[..n]).is_err() { ok = false; break; }
                                                rem -= n;
                                            }
                                        }
                                    }
                                    if ok {
                                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
                                        return;
                                    }
                                }
                                let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            }
                            _ => {
                                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            }
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    Ok((port, stop))
}

/// One-time initializer (`bh sandbox init-vm`) for Apple Silicon macOS:
/// 1. Compiles & signs native `berth-vz` (`Virtualization.framework`).
/// 2. Downloads official Debian 13 (`trixie`) `nocloud` ARM64 raw disk image, expands sparse disk to 8 GiB.
/// 3. Boots ONCE via `VZLinuxBootLoader` (`console=hvc0`) with temporary NAT networking, installs
///    Linux ARM64 runtime packages (`tzdata`, `locales`, `git`, `python3`, `ripgrep`, `jq`, `nodejs`,
///    full `linux-image-arm64`, and optionally Linux ARM64 `claude`, `codex`, `agy`), installs
///    `/usr/local/bin/berth-vm-init`, and exports Debian's own matched `vmlinux` + `initrd.img`.
pub fn init_macos_vm(
    state_dir: &Path,
    force: bool,
    install_agents: bool,
) -> Result<MacosVmStatus, BerthError> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (state_dir, force, install_agents);
        return Err(BerthError::StrictIsolationUnavailable(
            "`bh sandbox init-vm` is supported exclusively on Apple Silicon macOS (aarch64-apple-darwin)".into(),
        ));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        use std::sync::atomic::Ordering;

        let vm_dir = vm_dir_for_state(state_dir);
        fs::create_dir_all(&vm_dir)?;
        let helper = ensure_vz_helper(&vm_dir)?;

        let rootfs_path = vm_dir.join(VM_ROOTFS_NAME);
        let kernel_path = vm_dir.join(VM_KERNEL_NAME);
        let initrd_path = vm_dir.join(VM_INITRD_NAME);
        let meta_path = vm_dir.join(VM_META_NAME);

        if !force && rootfs_path.is_file() && kernel_path.is_file() && initrd_path.is_file() {
            return Ok(probe_macos_vm(state_dir));
        }

        let work = tempfile::Builder::new().prefix("berth-vm-init-").tempdir()?;
        let setup_dir = work.path().join("setup");
        fs::create_dir_all(&setup_dir)?;

        if force || !rootfs_path.is_file() {
            let tarball = work.path().join("debian-13-nocloud-arm64.tar.xz");
            eprintln!("[berth-vm] downloading official Debian 13 (trixie) ARM64 cloud image...");
            let dl = Command::new("/usr/bin/curl")
                .args(["-fL", "--progress-bar", "-o"])
                .arg(&tarball)
                .arg(DEBIAN_NOCLOUD_ARM64_URL)
                .status()?;
            if !dl.success() {
                return Err(BerthError::Io(format!(
                    "failed to download Debian 13 ARM64 image from {DEBIAN_NOCLOUD_ARM64_URL}"
                )));
            }

            eprintln!("[berth-vm] extracting raw ARM64 disk image...");
            let extract_dir = work.path().join("extract");
            fs::create_dir_all(&extract_dir)?;
            let tar_st = Command::new("/usr/bin/tar")
                .args(["-xJf"])
                .arg(&tarball)
                .arg("-C")
                .arg(&extract_dir)
                .status()?;
            if !tar_st.success() {
                return Err(BerthError::Io(
                    "failed to extract debian-13-nocloud-arm64.tar.xz".into(),
                ));
            }
            let disk_raw = extract_dir.join("disk.raw");
            if !disk_raw.is_file() {
                return Err(BerthError::Io(
                    "extracted Debian cloud archive did not contain disk.raw".into(),
                ));
            }
            // Grow sparse raw image to 8 GiB (occupies only written blocks on APFS).
            let file = fs::OpenOptions::new().write(true).open(&disk_raw)?;
            file.set_len(8 * 1024 * 1024 * 1024)?;
            drop(file);

            let tmp_rootfs = vm_dir.join(format!("{VM_ROOTFS_NAME}.tmp-{}", std::process::id()));
            fs::copy(&disk_raw, &tmp_rootfs)?;
            fs::rename(&tmp_rootfs, &rootfs_path)?;
        }

        // Download lightweight bootstrap ARM64 kernel + initrd solely for the one-time setup boot
        // (avoids EFI/GRUB ttyAMA0 mismatch and guarantees deterministic `console=hvc0` on Apple Silicon).
        let boot_vmlinuz_gz = work.path().join("bootstrap-vmlinuz.gz");
        let boot_vmlinux = work.path().join("bootstrap-vmlinux");
        let boot_initrd = work.path().join("bootstrap-initrd");
        eprintln!("[berth-vm] fetching one-time ARM64 bootstrap kernel/initrd...");
        for (url, dst) in [
            (BOOTSTRAP_KERNEL_URL, &boot_vmlinuz_gz),
            (BOOTSTRAP_INITRD_URL, &boot_initrd),
        ] {
            let dl = Command::new("/usr/bin/curl")
                .args(["-fsSL", "-o"])
                .arg(dst)
                .arg(url)
                .status()?;
            if !dl.success() {
                return Err(BerthError::Io(format!("failed to download {url}")));
            }
        }
        let gunzip_out = Command::new("/usr/bin/gzip")
            .args(["-dc"])
            .arg(&boot_vmlinuz_gz)
            .output()?;
        if gunzip_out.status.success() && !gunzip_out.stdout.is_empty() {
            fs::write(&boot_vmlinux, &gunzip_out.stdout)?;
        } else {
            fs::copy(&boot_vmlinuz_gz, &boot_vmlinux)?;
        }

        let setup_token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let setup_script = build_guest_setup_script(install_agents);
        let (setup_port, stop_srv) =
            start_vm_setup_http_server(setup_dir.clone(), setup_script, setup_token.clone())?;

        eprintln!("[berth-vm] booting Debian 13 ARM64 setup VM (VZLinuxBootLoader, console=hvc0)...");
        let st = Command::new(&helper)
            .arg("--linux-setup")
            .arg("--kernel")
            .arg(&boot_vmlinux)
            .arg("--initrd")
            .arg(&boot_initrd)
            .arg("--cmdline")
            .arg("console=hvc0 root=/dev/vda1 rw rootwait init=/bin/sh")
            .arg("--disk")
            .arg(&rootfs_path)
            .arg("--setup-port")
            .arg(setup_port.to_string())
            .arg("--setup-token")
            .arg(&setup_token)
            .arg("--setup-marker")
            .arg(setup_dir.join("SETUP_OK"))
            .arg("--memory-mb")
            .arg("3072")
            .status()?;

        stop_srv.store(true, Ordering::Relaxed);

        if !st.success() || !setup_dir.join("SETUP_OK").is_file() {
            return Err(BerthError::Io(format!(
                "ARM64 Linux Micro-VM setup failed (exit={:?}); re-run with `bh sandbox init-vm --force`",
                st.code()
            )));
        }

        let exported_kernel = setup_dir.join(VM_KERNEL_NAME);
        let exported_initrd = setup_dir.join(VM_INITRD_NAME);
        if !exported_kernel.is_file() || !exported_initrd.is_file() {
            return Err(BerthError::Io(
                "VM setup finished without exporting vmlinux and initrd.img".into(),
            ));
        }

        let tmp_k = vm_dir.join(format!("{VM_KERNEL_NAME}.tmp-{}", std::process::id()));
        let tmp_i = vm_dir.join(format!("{VM_INITRD_NAME}.tmp-{}", std::process::id()));
        fs::copy(&exported_kernel, &tmp_k)?;
        fs::copy(&exported_initrd, &tmp_i)?;
        fs::rename(&tmp_k, &kernel_path)?;
        fs::rename(&tmp_i, &initrd_path)?;

        let root_part = fs::read_to_string(setup_dir.join("ROOT_PART"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("/dev/"))
            .unwrap_or_else(|| "/dev/vda1".into());
        let meta = MacosVmMeta {
            os: "Linux".into(),
            os_version: "Debian 13".into(),
            arch: "arm64".into(),
            root_part,
        };
        let meta_bytes = serde_json::to_vec_pretty(&meta)?;
        write_private_file_atomic(&meta_path, &meta_bytes)?;

        eprintln!("[berth-vm] ARM64 Linux Micro-VM initialized and ready!");
        Ok(probe_macos_vm(state_dir))
    }
}

/// Launches the target CLI inside the hardware-isolated Apple Silicon ARM64 Linux Micro-VM
/// (zero `virtio-net`, RO `rootfs.raw` + tmpfs `/etc` overlay, VirtioFS `/workspace` & `/home/dev`,
/// `virtio-vsock` bridge to host `CageShim` & `ForwardProxy`).
pub fn launch_macos_vm(
    spec: &SandboxLaunchSpec,
    shim: &mut CageShimHandle,
) -> Result<ExitStatus, BerthError> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (spec, shim);
        return Err(BerthError::StrictIsolationUnavailable(
            "`--isolation vm` is supported exclusively on Apple Silicon macOS (aarch64-apple-darwin)".into(),
        ));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        use std::os::unix::fs::MetadataExt;

        validate_sandbox_workspace(&spec.workspace_dir, &spec.real_home, &spec.state_dir)?;
        let vm_dir = vm_dir_for_state(&spec.state_dir);
        let status = probe_macos_vm(&spec.state_dir);
        if !status.initialized {
            return Err(BerthError::StrictIsolationUnavailable(format!(
                "Apple Silicon ARM64 Linux Micro-VM is not initialized at {}; run once: `bh sandbox init-vm`",
                vm_dir.display()
            )));
        }
        // Keep helper compiled in sync with current bh binary automatically.
        let helper = ensure_vz_helper(&vm_dir)?;
        let meta = read_vm_guest_meta(&spec.state_dir);

        // Pre-accept `/workspace` trust inside isolated Cage home so interactive CLIs never stall.
        if spec.agent == "claude" {
            let _ = write_claude_sandbox_config(&spec.account_home, Path::new("/workspace"));
        } else if spec.agent == "codex" {
            let _ = write_codex_sandbox_config(
                &spec.account_home,
                &spec.cage,
                &shim.listen_url(),
                &shim.proxy_http_url(),
                &shim.proxy_socks5_url(),
                Some(Path::new("/workspace")),
            );
        }

        let owner_meta = fs::metadata(&spec.account_home.home_dir)?;
        let uid = owner_meta.uid();
        let gid = owner_meta.gid();

        let launch_cfg = build_vm_launch_config(
            spec,
            shim.listen_addr().port(),
            shim.proxy_addr().port(),
            shim.shim_token(),
            uid,
            gid,
        );

        let etc_tmp = tempfile::Builder::new()
            .prefix("berth-vm-etc-")
            .tempdir()?;
        let cfg_bytes = serde_json::to_vec_pretty(&launch_cfg)?;
        write_private_file_atomic(&etc_tmp.path().join("launch.json"), &cfg_bytes)?;
        write_private_file_atomic(
            &etc_tmp.path().join("vm_init.py"),
            GUEST_VM_INIT_PY.as_bytes(),
        )?;

        let console_log = etc_tmp.path().join("console.log");
        let vz_args = build_vz_runtime_args(
            &vm_dir,
            &meta,
            &spec.workspace_dir,
            &spec.account_home.home_dir,
            etc_tmp.path(),
            shim.listen_addr().port(),
            shim.proxy_addr().port(),
            &console_log,
        );

        let mut cmd = Command::new(&helper);
        cmd.args(&vz_args);
        let child = cmd.spawn()?;
        let exit = wait_child_or_rebind(child, shim, spec.auto_rebind)?;
        if !exit.success() && std::env::var_os("BERTH_VM_DEBUG").is_some() {
            if let Ok(log) = fs::read_to_string(&console_log) {
                eprintln!("[berth-vm] console log:\n{log}");
            }
        }
        Ok(exit)
    }
}

#[allow(dead_code)]
fn build_guest_setup_script(install_agents: bool) -> String {
    let install_flag = if install_agents { "1" } else { "0" };
    format!(
        r####"#!/bin/bash
set -euo pipefail
SRV="$1"
TOK="$2"
export DEBIAN_FRONTEND=noninteractive

on_setup_err() {{
  local rc=$?
  echo "[setup] FATAL error at line $1 (rc=$rc): $2"
  sync
  mount -o remount,ro / 2>/dev/null || true
  poweroff -f
}}
trap 'on_setup_err "$LINENO" "$BASH_COMMAND"' ERR

upload_file() {{
  local name="$1"
  local src="$2"
  curl -fsSL -X PUT -H "X-Setup-Token: $TOK" --data-binary "@$src" "$SRV/upload/$name" >/dev/null
}}

echo "[setup] expanding root partition and filesystem..."
ROOT_SRC=$(findmnt -n -o SOURCE /)
echo "$ROOT_SRC" > /tmp/ROOT_PART
DISK_DEV=$(lsblk -no pkname "$ROOT_SRC" | head -n1 || true)
PART_NUM=$(cat "/sys/class/block/$(basename "$ROOT_SRC")/partition" 2>/dev/null || echo 1)
if [ -n "$DISK_DEV" ]; then
  if command -v growpart >/dev/null 2>&1; then
    growpart "/dev/$DISK_DEV" "$PART_NUM" || true
  fi
  resize2fs "$ROOT_SRC" || true
fi

echo "[setup] ensuring real disk /lib/modules is exposed (lazy-unmounting any bootstrap initrd overmount)..."
findmnt -n /lib/modules /usr/lib/modules 2>/dev/null || true
umount -l /lib/modules 2>/dev/null || true
umount -l /usr/lib/modules 2>/dev/null || true
echo "[setup] /boot before apt: $(ls -1 /boot | tr '\n' ' ')"
echo "[setup] /lib/modules after umount -l: $(ls -1 /lib/modules 2>/dev/null | tr '\n' ' ')"

echo "[setup] installing base packages & full linux-image-arm64 (virtiofs + vsock + overlay)..."
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates curl git python3 ripgrep jq tzdata locales \
  iproute2 procps xz-utils tar gzip bzip2 unzip kmod initramfs-tools \
  cloud-guest-utils e2fsprogs linux-image-arm64

# Retry partition growth now that cloud-guest-utils (growpart) is guaranteed installed.
if [ -n "$DISK_DEV" ] && command -v growpart >/dev/null 2>&1; then
  growpart "/dev/$DISK_DEV" "$PART_NUM" || true
  resize2fs "$ROOT_SRC" || true
fi

# Ensure UTF-8 locales commonly used by Berth cohorts are available.
for loc in en_US.UTF-8 en_SG.UTF-8 en_GB.UTF-8 en_AU.UTF-8 ja_JP.UTF-8; do
  sed -i "s/^# *${{loc}}/${{loc}}/" /etc/locale.gen || true
done
locale-gen >/dev/null 2>&1 || true

# Include virtio PCI/MMIO/blk/console/fs/vsock + overlay + ext4 in Debian initramfs for <1s boot.
mkdir -p /etc/initramfs-tools
touch /etc/initramfs-tools/modules
for m in virtio_pci virtio_mmio virtio_blk virtio_console virtiofs vmw_vsock_virtio_transport overlay ext4; do
  grep -qx "$m" /etc/initramfs-tools/modules 2>/dev/null || echo "$m" >> /etc/initramfs-tools/modules
done
echo "[setup] /boot kernels: $(ls -1 /boot/vmlinuz-* 2>/dev/null | tr '\n' ' ')"
echo "[setup] /lib/modules after apt: $(ls -1 /lib/modules 2>/dev/null | tr '\n' ' ')"
TARGET_KVER=$( (for v in /boot/vmlinuz-*; do [ -f "$v" ] && basename "$v" | sed 's/^vmlinuz-//'; done) | grep -v cloud | sort -V | tail -n1 || true)
if [ -z "$TARGET_KVER" ]; then
  TARGET_KVER=$( (for v in /boot/vmlinuz-*; do [ -f "$v" ] && basename "$v" | sed 's/^vmlinuz-//'; done) | sort -V | tail -n1)
fi
if [ ! -d "/lib/modules/$TARGET_KVER" ]; then
  echo "[setup] /lib/modules/$TARGET_KVER missing; reinstalling linux-image-$TARGET_KVER..."
  apt-get install -y -qq --reinstall "linux-image-${{TARGET_KVER}}"
fi
test -d "/lib/modules/$TARGET_KVER"
echo "[setup] building initramfs for Debian kernel $TARGET_KVER..."
find "/lib/modules/$TARGET_KVER" \( -name '*virtiofs*' -o -name '*vsock*' -o -name 'overlay.ko*' -o -name '*virtio_blk*' \) 2>/dev/null | head -n 12 || true
update-initramfs -u -k "$TARGET_KVER" || update-initramfs -c -k "$TARGET_KVER"

# Install Node.js 22 LTS (required by official Codex & Claude tooling).
if ! command -v node >/dev/null 2>&1 || [ "$(node -v | sed 's/v//' | cut -d. -f1)" -lt 20 ]; then
  echo "[setup] installing Node.js 22 LTS (arm64)..."
  curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null
  apt-get install -y -qq nodejs
fi

mkdir -p /opt/berth/bin /workspace /home/dev /run/berth-etc
curl -fsSL -H "X-Setup-Token: $TOK" "$SRV/berth-vm-init" -o /usr/local/bin/berth-vm-init
chmod 0755 /usr/local/bin/berth-vm-init

if [ "{install_flag}" = "1" ]; then
  echo "[setup] installing Linux ARM64 official CLIs (codex, claude, agy)..."
  npm install -g @openai/codex >/dev/null 2>&1 || echo "[setup] warn: @openai/codex npm install failed"
  if ! command -v claude >/dev/null 2>&1; then
    npm install -g @anthropic-ai/claude-code >/dev/null 2>&1 || \
      echo "[setup] warn: @anthropic-ai/claude-code npm install failed"
  fi
  if ! PATH="/opt/berth/bin:$PATH" command -v agy >/dev/null 2>&1 && [ ! -x /opt/berth/.local/bin/agy ]; then
    (curl -fsSL https://antigravity.google/cli/install.sh | HOME=/opt/berth bash) || \
      echo "[setup] warn: agy install failed"
  fi
  if [ -x /opt/berth/.local/bin/agy ]; then
    ln -sf /opt/berth/.local/bin/agy /usr/local/bin/agy
  fi
  echo "[setup] installed CLI versions:"
  (PATH="/usr/local/bin:/usr/bin:/bin" command -v claude && PATH="/usr/local/bin:/usr/bin:/bin" claude --version) || true
  (PATH="/usr/local/bin:/usr/bin:/bin" command -v codex && PATH="/usr/local/bin:/usr/bin:/bin" codex --version) || true
  (PATH="/usr/local/bin:/usr/bin:/bin" command -v agy && PATH="/usr/local/bin:/usr/bin:/bin" agy --version) || true
fi

# Clean apt caches and one-time setup unit so the finalized rootfs is spotless.
rm -f /etc/systemd/system/multi-user.target.wants/berth-setup.service \
      /etc/systemd/system/berth-setup.service \
      /usr/local/sbin/berth-run-setup.sh
apt-get clean
rm -rf /var/lib/apt/lists/*

# Export uncompressed Debian ARM64 Image (`vmlinux`) and matching `initrd.img` to macOS host.
VMLINUZ="/boot/vmlinuz-${{TARGET_KVER}}"
INITRD="/boot/initrd.img-${{TARGET_KVER}}"
echo "[setup] exporting Debian kernel $VMLINUZ and initrd $INITRD..."
if gzip -t "$VMLINUZ" 2>/dev/null; then
  gzip -dc "$VMLINUZ" > /tmp/vmlinux.out
else
  cp "$VMLINUZ" /tmp/vmlinux.out
fi
upload_file "ROOT_PART" "/tmp/ROOT_PART"
upload_file "vmlinux" "/tmp/vmlinux.out"
upload_file "initrd.img" "$INITRD"
echo "OK" > /tmp/SETUP_OK
upload_file "SETUP_OK" "/tmp/SETUP_OK"
rm -f /tmp/vmlinux.out /tmp/ROOT_PART /tmp/SETUP_OK

echo "[setup] syncing and remounting root read-only before poweroff..."
sync
mount -o remount,ro / 2>/dev/null || true
poweroff -f
"####
    )
}

#[allow(dead_code)]
const VZ_ENTITLEMENTS_PLIST: &str = r####"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.virtualization</key>
    <true/>
</dict>
</plist>
"####;

/// Static 12-line PID 1 bootstrap installed inside `rootfs.raw` at `/usr/local/bin/berth-vm-init`.
/// Delegates immediately to `/run/berth-etc/vm_init.py` (mounted RO from host) so updating `bh`
/// updates guest runtime behavior instantaneously with zero image rebuilds.
#[allow(dead_code)]
const GUEST_VM_INIT_SH: &str = r####"#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t tmpfs tmpfs /run 2>/dev/null || true
modprobe virtiofs 2>/dev/null || true
modprobe overlay 2>/dev/null || true
modprobe vmw_vsock_virtio_transport 2>/dev/null || true
mkdir -p /run/berth-etc
mount -t virtiofs -o ro cageetc /run/berth-etc
exec python3 -u /run/berth-etc/vm_init.py
"####;

/// Guest PID 1 runtime orchestrator (executed from read-only VirtioFS `/run/berth-etc/vm_init.py`).
/// Sets up tmpfs overlayfs on `/etc` & `/var`, mounts `/workspace` & `/home/dev`, enforces
/// deterministic Cage identity (`hostname`, `localtime`, `locale`, `machine-id`, `passwd`, `lo`),
/// bridges `127.0.0.1:<shim_port|proxy_port>` over `AF_VSOCK` to host `CageShim`/`ForwardProxy`,
/// and runs the target CLI with full PTY/stdio + resize + signal + exit-code framing over `vsock:5003`.
#[allow(dead_code)]
const GUEST_VM_INIT_PY: &str = r####"#!/usr/bin/env python3
import ctypes
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import termios
import threading

AF_VSOCK = getattr(socket, "AF_VSOCK", 40)
VMADDR_CID_HOST = 2
LINUX_REBOOT_MAGIC1 = 0xFEE1DEAD
LINUX_REBOOT_MAGIC2 = 672274793
LINUX_REBOOT_CMD_POWER_OFF = 0x4321FEDC

FRAME_STDIN = 1
FRAME_STDOUT = 2
FRAME_STDERR = 3
FRAME_RESIZE = 4
FRAME_SIGNAL = 5
FRAME_EXIT = 6

libc = ctypes.CDLL("libc.so.6", use_errno=True)


def poweroff_now():
    try:
        os.sync()
    except Exception:
        pass
    try:
        libc.reboot(LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, LINUX_REBOOT_CMD_POWER_OFF, None)
    except Exception:
        pass
    subprocess.run(["/sbin/poweroff", "-f"], check=False)
    os._exit(0)


def run_cmd(argv):
    subprocess.run(argv, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def setup_filesystems(cfg):
    os.makedirs("/dev/pts", exist_ok=True)
    os.makedirs("/dev/shm", exist_ok=True)
    os.makedirs("/tmp", exist_ok=True)
    run_cmd(["mount", "-t", "devpts", "devpts", "/dev/pts"])
    run_cmd(["mount", "-t", "tmpfs", "tmpfs", "/dev/shm"])
    run_cmd(["mount", "-t", "tmpfs", "-o", "mode=1777", "tmpfs", "/tmp"])
    os.makedirs("/tmp/runtime", mode=0o700, exist_ok=True)
    os.chown("/tmp/runtime", cfg["uid"], cfg["gid"])

    # Tmpfs overlayfs on /etc and /var over read-only rootfs.raw (concurrent-safe!).
    for name in ("etc", "var"):
        upper = f"/run/ovl-{name}-upper"
        work = f"/run/ovl-{name}-work"
        os.makedirs(upper, exist_ok=True)
        os.makedirs(work, exist_ok=True)
        run_cmd([
            "mount", "-t", "overlay", "overlay",
            "-o", f"lowerdir=/{name},upperdir={upper},workdir={work}",
            f"/{name}",
        ])

    os.makedirs("/workspace", exist_ok=True)
    os.makedirs("/home/dev", exist_ok=True)
    if subprocess.run(["mount", "-t", "virtiofs", "workspace", "/workspace"]).returncode != 0:
        raise RuntimeError("failed to mount virtiofs workspace")
    if subprocess.run(["mount", "-t", "virtiofs", "cagehome", "/home/dev"]).returncode != 0:
        raise RuntimeError("failed to mount virtiofs cagehome")


def apply_cage_identity(cfg):
    hostname = cfg["hostname"]
    tz = cfg["timezone"]
    locale = cfg["locale"]
    mid = cfg["machine_id"]
    uid = cfg["uid"]
    gid = cfg["gid"]

    # Set kernel UTS hostname directly via sethostname(2).
    hb = hostname.encode("utf-8")
    libc.sethostname(hb, len(hb))

    def write_replacing(path, content):
        if os.path.lexists(path):
            try:
                os.unlink(path)
            except IsADirectoryError:
                pass
        with open(path, "w") as f:
            f.write(content)

    write_replacing("/etc/hostname", hostname + "\n")
    write_replacing("/etc/hosts", f"127.0.0.1 localhost {hostname}\n")
    write_replacing("/etc/machine-id", mid + "\n")
    if os.path.isdir("/var/lib/dbus"):
        write_replacing("/var/lib/dbus/machine-id", mid + "\n")
    write_replacing("/etc/timezone", tz + "\n")
    zone = os.path.join("/usr/share/zoneinfo", tz)
    if os.path.isfile(zone):
        if os.path.lexists("/etc/localtime"):
            os.unlink("/etc/localtime")
        os.symlink(zone, "/etc/localtime")
    write_replacing("/etc/locale.conf", f"LANG={locale}\nLC_ALL={locale}\n")
    os.makedirs("/etc/default", exist_ok=True)
    write_replacing("/etc/default/locale", f"LANG={locale}\nLC_ALL={locale}\n")
    write_replacing("/etc/resolv.conf", "# Managed local resolver\nnameserver 127.0.0.1\n")
    write_replacing(
        "/etc/passwd",
        f"root:x:0:0::/root:/bin/bash\ndev:x:{uid}:{gid}::/home/dev:/bin/bash\nnobody:x:65534:65534::/nonexistent:/usr/sbin/nologin\n",
    )
    write_replacing(
        "/etc/group",
        f"root:x:0:\ndev:x:{gid}:\nnogroup:x:65534:\n",
    )

    # Bring up loopback lo ONLY (zero external NICs exist).
    run_cmd(["ip", "link", "set", "lo", "up"])
    run_cmd(["ip", "addr", "add", "127.0.0.1/8", "dev", "lo"])


def pipe_sockets(a, b):
    def copy_one(src, dst):
        try:
            while True:
                data = src.recv(16384)
                if not data:
                    break
                dst.sendall(data)
        except Exception:
            pass
        finally:
            try:
                dst.shutdown(socket.SHUT_WR)
            except Exception:
                pass

    t1 = threading.Thread(target=copy_one, args=(a, b), daemon=True)
    t2 = threading.Thread(target=copy_one, args=(b, a), daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    for s in (a, b):
        try:
            s.close()
        except Exception:
            pass


def start_tcp_vsock_bridge(tcp_port, vsock_port):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", tcp_port))
    srv.listen(64)

    def accept_loop():
        while True:
            try:
                cli, _ = srv.accept()
                cli.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except Exception:
                break

            def handle(conn):
                try:
                    vs = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
                    vs.connect((VMADDR_CID_HOST, vsock_port))
                except Exception:
                    try:
                        conn.close()
                    except Exception:
                        pass
                    return
                pipe_sockets(conn, vs)

            threading.Thread(target=handle, args=(cli,), daemon=True).start()

    threading.Thread(target=accept_loop, daemon=True).start()


def send_frame(sock, lock, ftype, payload=b""):
    hdr = struct.pack(">BI", ftype, len(payload))
    with lock:
        sock.sendall(hdr + payload)


def recv_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def run_agent_session(cfg):
    ctl = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
    ctl.connect((VMADDR_CID_HOST, cfg["vsock_pty_port"]))
    ctl_lock = threading.Lock()

    # Handshake from host: [is_tty: u8][rows: u16 BE][cols: u16 BE]
    hello = recv_exact(ctl, 5)
    if not hello:
        return 1
    is_tty, rows, cols = struct.unpack(">BHH", hello)

    cmd = cfg["command"]
    env = dict(cfg["env"])
    uid = cfg["uid"]
    gid = cfg["gid"]

    # Resolve binary via guest PATH (also check /opt/berth/bin without exposing it in guest PATH).
    search_path = env.get("PATH", "/usr/local/bin:/usr/bin:/bin")
    resolved = shutil.which(cmd[0], path=search_path) or shutil.which(cmd[0], path="/opt/berth/bin:/opt/berth/.local/bin")
    if not resolved:
        msg = f"[berth-vm] error: `{cmd[0]}` not found in Linux ARM64 VM PATH ({env.get('PATH')}); run `bh sandbox init-vm --force` or install `{cmd[0]}` in VM\n".encode()
        send_frame(ctl, ctl_lock, FRAME_STDERR, msg)
        send_frame(ctl, ctl_lock, FRAME_EXIT, bytes([127]))
        ctl.close()
        return 127
    argv = [resolved] + cmd[1:]

    def preexec():
        os.setsid()
        os.setgid(gid)
        os.setuid(uid)

    if is_tty:
        master_fd, slave_fd = pty.openpty()
        if rows > 0 and cols > 0:
            fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

        def pty_preexec():
            os.setsid()
            fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
            os.setgid(gid)
            os.setuid(uid)

        proc = subprocess.Popen(
            argv,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            cwd="/workspace",
            env=env,
            preexec_fn=pty_preexec,
            close_fds=True,
        )
        os.close(slave_fd)

        def pty_reader():
            try:
                while True:
                    r, _, _ = select.select([master_fd], [], [], 0.2)
                    if r:
                        data = os.read(master_fd, 16384)
                        if not data:
                            break
                        send_frame(ctl, ctl_lock, FRAME_STDOUT, data)
                    elif proc.poll() is not None:
                        break
            except OSError:
                pass

        rt = threading.Thread(target=pty_reader, daemon=True)
        rt.start()

        def host_reader():
            try:
                while True:
                    hdr = recv_exact(ctl, 5)
                    if not hdr:
                        break
                    ftype, flen = struct.unpack(">BI", hdr)
                    payload = recv_exact(ctl, flen) if flen > 0 else b""
                    if payload is None:
                        break
                    if ftype == FRAME_STDIN and payload:
                        os.write(master_fd, payload)
                    elif ftype == FRAME_RESIZE and len(payload) == 4:
                        r, c = struct.unpack(">HH", payload)
                        if r > 0 and c > 0:
                            fcntl.ioctl(master_fd, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0))
                            try:
                                os.killpg(proc.pid, signal.SIGWINCH)
                            except ProcessLookupError:
                                pass
                    elif ftype == FRAME_SIGNAL and len(payload) == 1:
                        sig = payload[0]
                        try:
                            os.killpg(proc.pid, sig)
                        except ProcessLookupError:
                            pass
            except Exception:
                pass

        threading.Thread(target=host_reader, daemon=True).start()
        rc = proc.wait()
        rt.join(timeout=1.0)
        try:
            os.close(master_fd)
        except OSError:
            pass
    else:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd="/workspace",
            env=env,
            preexec_fn=preexec,
            close_fds=True,
        )

        def pump_pipe(pipe, ftype):
            try:
                while True:
                    data = pipe.read1(16384) if hasattr(pipe, "read1") else pipe.read(4096)
                    if not data:
                        break
                    send_frame(ctl, ctl_lock, ftype, data)
            except Exception:
                pass

        t_out = threading.Thread(target=pump_pipe, args=(proc.stdout, FRAME_STDOUT), daemon=True)
        t_err = threading.Thread(target=pump_pipe, args=(proc.stderr, FRAME_STDERR), daemon=True)
        t_out.start()
        t_err.start()

        def host_reader():
            try:
                while True:
                    hdr = recv_exact(ctl, 5)
                    if not hdr:
                        break
                    ftype, flen = struct.unpack(">BI", hdr)
                    payload = recv_exact(ctl, flen) if flen > 0 else b""
                    if payload is None:
                        break
                    if ftype == FRAME_STDIN:
                        if not payload:
                            try:
                                proc.stdin.close()
                            except Exception:
                                pass
                        else:
                            try:
                                proc.stdin.write(payload)
                                proc.stdin.flush()
                            except Exception:
                                pass
                    elif ftype == FRAME_SIGNAL and len(payload) == 1:
                        try:
                            os.killpg(proc.pid, payload[0])
                        except ProcessLookupError:
                            pass
            except Exception:
                pass

        threading.Thread(target=host_reader, daemon=True).start()
        rc = proc.wait()
        t_out.join(timeout=2.0)
        t_err.join(timeout=2.0)

    exit_code = rc if rc >= 0 else (128 + (-rc)) & 0xFF
    try:
        send_frame(ctl, ctl_lock, FRAME_EXIT, bytes([exit_code & 0xFF]))
        ctl.shutdown(socket.SHUT_WR)
        ctl.close()
    except Exception:
        pass
    return exit_code


def main():
    try:
        with open("/run/berth-etc/launch.json", "r") as f:
            cfg = json.load(f)
        setup_filesystems(cfg)
        apply_cage_identity(cfg)
        start_tcp_vsock_bridge(cfg["shim_port"], cfg["vsock_shim_port"])
        start_tcp_vsock_bridge(cfg["proxy_port"], cfg["vsock_proxy_port"])
        run_agent_session(cfg)
    except Exception as exc:
        sys.stderr.write(f"[berth-vm-init] fatal: {exc}\n")
        sys.stderr.flush()
    finally:
        poweroff_now()


if __name__ == "__main__":
    main()
"####;

/// Self-contained Apple Silicon `Virtualization.framework` runner (`berth-vz`).
/// Supports two explicit modes:
/// - Runtime zero-network mode (`--kernel` + `--initrd` + `--disk-ro` + `--vsock-forward` + `--vsock-pty-port`):
///   attaches ZERO network devices, mounts strictly requested VirtioFS shares, and bridges
///   `CageShim`/`ForwardProxy` + interactive PTY/stdio over `virtio-vsock`.
/// - One-time setup mode (`--efi-setup` + `--efi-vars` + `--setup-marker`):
///   boots Debian 13 `nocloud` ARM64 via `VZEFIBootLoader` with temporary NAT networking,
///   automates passwordless root login on `hvc0`, runs `/mnt/setup/setup.sh`, and exits on poweroff.
#[allow(dead_code)]
const BERTH_VZ_SWIFT_SRC: &str = r####"import Foundation
import Virtualization
import Darwin

// ---------------- CLI Argument Parsing ----------------

struct ShareSpec {
    let tag: String
    let path: String
    let readOnly: Bool
}

struct VsockForwardSpec {
    let vsockPort: UInt32
    let tcpHost: String
    let tcpPort: UInt16
}

var linuxSetup = false
var setupPort: String? = nil
var setupToken: String? = nil
var setupMarkerPath: String? = nil
var kernelPath: String? = nil
var initrdPath: String? = nil
var cmdline = "console=hvc0 root=/dev/vda1 ro rootflags=noload quiet init=/usr/local/bin/berth-vm-init"
var diskPath: String? = nil
var diskReadOnly = false
var shares: [ShareSpec] = []
var vsockForwards: [VsockForwardSpec] = []
var vsockPtyPort: UInt32? = nil
var consoleLogPath: String? = nil
var cpuCount = max(2, min(4, ProcessInfo.processInfo.processorCount))
var memoryMB: UInt64 = 2048

var i = 1
let argv = CommandLine.arguments
while i < argv.count {
    let arg = argv[i]
    func nextVal() -> String {
        i += 1
        guard i < argv.count else {
            fputs("berth-vz: missing value for \(arg)\n", stderr)
            exit(2)
        }
        return argv[i]
    }
    switch arg {
    case "--linux-setup": linuxSetup = true
    case "--setup-port": setupPort = nextVal()
    case "--setup-token": setupToken = nextVal()
    case "--setup-marker": setupMarkerPath = nextVal()
    case "--kernel": kernelPath = nextVal()
    case "--initrd": initrdPath = nextVal()
    case "--cmdline": cmdline = nextVal()
    case "--disk": diskPath = nextVal()
    case "--disk-ro": diskReadOnly = true
    case "--console-log": consoleLogPath = nextVal()
    case "--cpus": cpuCount = max(1, Int(nextVal()) ?? cpuCount)
    case "--memory-mb": memoryMB = max(512, UInt64(nextVal()) ?? memoryMB)
    case "--vsock-pty-port": vsockPtyPort = UInt32(nextVal())
    case "--share":
        let raw = nextVal()
        guard let eq = raw.firstIndex(of: "=") else {
            fputs("berth-vz: invalid --share \(raw)\n", stderr)
            exit(2)
        }
        let tag = String(raw[..<eq])
        var rest = String(raw[raw.index(after: eq)...])
        var ro = false
        if rest.hasSuffix(":ro") {
            ro = true
            rest = String(rest.dropLast(3))
        }
        shares.append(ShareSpec(tag: tag, path: rest, readOnly: ro))
    case "--vsock-forward":
        let raw = nextVal()
        let parts = raw.split(separator: "=", maxSplits: 1)
        guard parts.count == 2, let vp = UInt32(parts[0]) else {
            fputs("berth-vz: invalid --vsock-forward \(raw)\n", stderr)
            exit(2)
        }
        let hp = parts[1].split(separator: ":", maxSplits: 1)
        guard hp.count == 2, let tp = UInt16(hp[1]) else {
            fputs("berth-vz: invalid tcp target in \(raw)\n", stderr)
            exit(2)
        }
        vsockForwards.append(VsockForwardSpec(vsockPort: vp, tcpHost: String(hp[0]), tcpPort: tp))
    default:
        fputs("berth-vz: unknown flag \(arg)\n", stderr)
        exit(2)
    }
    i += 1
}

guard VZVirtualMachine.isSupported else {
    fputs("berth-vz: Apple Virtualization.framework is not supported on this host\n", stderr)
    exit(1)
}
guard let disk = diskPath else {
    fputs("berth-vz: --disk is required\n", stderr)
    exit(2)
}

// ---------------- POSIX Low-Level Helpers ----------------

let FRAME_STDIN: UInt8 = 1
let FRAME_STDOUT: UInt8 = 2
let FRAME_STDERR: UInt8 = 3
let FRAME_RESIZE: UInt8 = 4
let FRAME_SIGNAL: UInt8 = 5
let FRAME_EXIT: UInt8 = 6

func writeAll(fd: Int32, bytes: UnsafeRawPointer, count: Int) -> Bool {
    var written = 0
    while written < count {
        let n = Darwin.write(fd, bytes.advanced(by: written), count - written)
        if n > 0 {
            written += n
        } else if n < 0 && errno == EINTR {
            continue
        } else {
            return false
        }
    }
    return true
}

func writeAllData(fd: Int32, data: Data) -> Bool {
    if data.isEmpty { return true }
    return data.withUnsafeBytes { ptr in
        guard let base = ptr.baseAddress else { return true }
        return writeAll(fd: fd, bytes: base, count: data.count)
    }
}

func readExact(fd: Int32, count: Int) -> Data? {
    if count == 0 { return Data() }
    var buf = Data(count: count)
    var got = 0
    let ok = buf.withUnsafeMutableBytes { ptr -> Bool in
        guard let base = ptr.baseAddress else { return false }
        while got < count {
            let n = Darwin.read(fd, base.advanced(by: got), count - got)
            if n > 0 {
                got += n
            } else if n < 0 && errno == EINTR {
                continue
            } else {
                return false
            }
        }
        return true
    }
    return ok ? buf : nil
}

func connectLocalTCP(host: String, port: UInt16) -> Int32 {
    let fd = Darwin.socket(AF_INET, SOCK_STREAM, 0)
    if fd < 0 { return -1 }
    var one: Int32 = 1
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, socklen_t(MemoryLayout<Int32>.size))
    var addr = sockaddr_in()
    addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = port.bigEndian
    if inet_pton(AF_INET, host, &addr.sin_addr) != 1 {
        Darwin.close(fd)
        return -1
    }
    let res = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    if res != 0 {
        Darwin.close(fd)
        return -1
    }
    return fd
}

// ---------------- Terminal State Management ----------------

var savedTermios: termios? = nil
let termiosLock = NSLock()

func restoreTermios() {
    termiosLock.lock()
    defer { termiosLock.unlock() }
    if var orig = savedTermios {
        tcsetattr(STDIN_FILENO, TCSANOW, &orig)
        savedTermios = nil
    }
}

func enableRawTerminal() {
    termiosLock.lock()
    defer { termiosLock.unlock() }
    var orig = termios()
    if tcgetattr(STDIN_FILENO, &orig) == 0 {
        savedTermios = orig
        var raw = orig
        cfmakeraw(&raw)
        tcsetattr(STDIN_FILENO, TCSANOW, &raw)
    }
}

func currentTermSize() -> (UInt16, UInt16) {
    var ws = winsize()
    if ioctl(STDIN_FILENO, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        return (ws.ws_row, ws.ws_col)
    }
    return (24, 80)
}

// ---------------- Vsock Forwarder (Guest -> Host Loopback TCP) ----------------

final class VsockForwardDelegate: NSObject, VZVirtioSocketListenerDelegate {
    let host: String
    let port: UInt16
    let lock = NSLock()
    var activeConns: [ObjectIdentifier: VZVirtioSocketConnection] = [:]

    init(host: String, port: UInt16) {
        self.host = host
        self.port = port
    }

    func listener(
        _ listener: VZVirtioSocketListener,
        shouldAcceptNewConnection connection: VZVirtioSocketConnection,
        from socketDevice: VZVirtioSocketDevice
    ) -> Bool {
        let tcpFd = connectLocalTCP(host: host, port: port)
        if tcpFd < 0 {
            return false
        }
        let vsFd = connection.fileDescriptor
        let cid = ObjectIdentifier(connection)
        lock.lock()
        activeConns[cid] = connection
        lock.unlock()

        let group = DispatchGroup()
        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            var buf = [UInt8](repeating: 0, count: 16384)
            while true {
                let n = Darwin.read(vsFd, &buf, buf.count)
                if n > 0 {
                    if !writeAll(fd: tcpFd, bytes: buf, count: n) { break }
                } else if n < 0 && errno == EINTR {
                    continue
                } else {
                    break
                }
            }
            Darwin.shutdown(tcpFd, SHUT_WR)
            group.leave()
        }

        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            var buf = [UInt8](repeating: 0, count: 16384)
            while true {
                let n = Darwin.read(tcpFd, &buf, buf.count)
                if n > 0 {
                    if !writeAll(fd: vsFd, bytes: buf, count: n) { break }
                } else if n < 0 && errno == EINTR {
                    continue
                } else {
                    break
                }
            }
            Darwin.shutdown(vsFd, SHUT_WR)
            group.leave()
        }

        group.notify(queue: DispatchQueue.global()) { [weak self] in
            Darwin.close(tcpFd)
            self?.lock.lock()
            self?.activeConns.removeValue(forKey: cid)
            self?.lock.unlock()
        }
        return true
    }
}

// ---------------- Vsock PTY / Stdio Session Controller ----------------

final class VsockPtyDelegate: NSObject, VZVirtioSocketListenerDelegate {
    let isTty: Bool
    let sendLock = NSLock()
    var activeConn: VZVirtioSocketConnection? = nil
    var guestExitCode: Int32? = nil
    var onGuestExit: ((Int32) -> Void)? = nil

    init(isTty: Bool) {
        self.isTty = isTty
    }

    func sendFrame(type: UInt8, payload: Data) {
        sendLock.lock()
        defer { sendLock.unlock() }
        guard let conn = activeConn else { return }
        var hdr = Data(capacity: 5)
        hdr.append(type)
        var lenBE = UInt32(payload.count).bigEndian
        withUnsafeBytes(of: &lenBE) { hdr.append(contentsOf: $0) }
        _ = writeAllData(fd: conn.fileDescriptor, data: hdr)
        if !payload.isEmpty {
            _ = writeAllData(fd: conn.fileDescriptor, data: payload)
        }
    }

    func sendResize() {
        guard isTty else { return }
        let (rows, cols) = currentTermSize()
        var payload = Data(capacity: 4)
        var rBE = rows.bigEndian
        var cBE = cols.bigEndian
        withUnsafeBytes(of: &rBE) { payload.append(contentsOf: $0) }
        withUnsafeBytes(of: &cBE) { payload.append(contentsOf: $0) }
        sendFrame(type: FRAME_RESIZE, payload: payload)
    }

    func sendSignal(_ sig: UInt8) {
        sendFrame(type: FRAME_SIGNAL, payload: Data([sig]))
    }

    func listener(
        _ listener: VZVirtioSocketListener,
        shouldAcceptNewConnection connection: VZVirtioSocketConnection,
        from socketDevice: VZVirtioSocketDevice
    ) -> Bool {
        sendLock.lock()
        activeConn = connection
        sendLock.unlock()

        let fd = connection.fileDescriptor
        let (rows, cols) = isTty ? currentTermSize() : (0, 0)
        var hello = Data(capacity: 5)
        hello.append(isTty ? 1 : 0)
        var rBE = rows.bigEndian
        var cBE = cols.bigEndian
        withUnsafeBytes(of: &rBE) { hello.append(contentsOf: $0) }
        withUnsafeBytes(of: &cBE) { hello.append(contentsOf: $0) }
        _ = writeAllData(fd: fd, data: hello)

        if isTty {
            enableRawTerminal()
        }

        // Pump host stdin -> guest FRAME_STDIN.
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var buf = [UInt8](repeating: 0, count: 4096)
            while true {
                let n = Darwin.read(STDIN_FILENO, &buf, buf.count)
                if n > 0 {
                    self?.sendFrame(type: FRAME_STDIN, payload: Data(buf[0..<n]))
                } else if n < 0 && errno == EINTR {
                    continue
                } else {
                    self?.sendFrame(type: FRAME_STDIN, payload: Data())
                    break
                }
            }
        }

        // Pump guest frames -> host stdout / stderr / exit.
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            while let hdr = readExact(fd: fd, count: 5) {
                let ftype = hdr[0]
                let flen = Int(
                    (UInt32(hdr[1]) << 24) | (UInt32(hdr[2]) << 16) |
                    (UInt32(hdr[3]) << 8)  | UInt32(hdr[4])
                )
                guard let payload = readExact(fd: fd, count: flen) else { break }
                switch ftype {
                case FRAME_STDOUT:
                    _ = writeAllData(fd: STDOUT_FILENO, data: payload)
                case FRAME_STDERR:
                    _ = writeAllData(fd: STDERR_FILENO, data: payload)
                case FRAME_EXIT:
                    let code: Int32 = payload.isEmpty ? 0 : Int32(payload[0])
                    restoreTermios()
                    self?.guestExitCode = code
                    self?.onGuestExit?(code)
                    return
                default:
                    break
                }
            }
            restoreTermios()
        }
        return true
    }
}

// ---------------- VM Lifecycle Delegate ----------------

final class VMLifecycleDelegate: NSObject, VZVirtualMachineDelegate {
    var onStop: ((Error?) -> Void)? = nil
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        onStop?(nil)
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        onStop?(error)
    }
}

// ---------------- Configure Virtual Machine ----------------

let cfg = VZVirtualMachineConfiguration()
cfg.cpuCount = min(max(cpuCount, VZVirtualMachineConfiguration.minimumAllowedCPUCount), VZVirtualMachineConfiguration.maximumAllowedCPUCount)
cfg.memorySize = min(max(memoryMB * 1024 * 1024, VZVirtualMachineConfiguration.minimumAllowedMemorySize), VZVirtualMachineConfiguration.maximumAllowedMemorySize)
cfg.platform = VZGenericPlatformConfiguration()
cfg.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

guard let kp = kernelPath else {
    fputs("berth-vz: --kernel is required\n", stderr)
    exit(2)
}
let boot = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kp))
if let ip = initrdPath {
    boot.initialRamdiskURL = URL(fileURLWithPath: ip)
}
boot.commandLine = cmdline
cfg.bootLoader = boot

if linuxSetup {
    // Temporary NAT network ONLY during one-time `--linux-setup` initialization.
    let net = VZVirtioNetworkDeviceConfiguration()
    net.attachment = VZNATNetworkDeviceAttachment()
    cfg.networkDevices = [net]
} else {
    // ZERO network devices in runtime mode — kernel-level egress isolation by hardware design!
    cfg.networkDevices = []
}

let diskAttachment: VZDiskImageStorageDeviceAttachment
do {
    diskAttachment = try VZDiskImageStorageDeviceAttachment(
        url: URL(fileURLWithPath: disk),
        readOnly: diskReadOnly
    )
} catch {
    fputs("berth-vz: failed to attach disk \(disk): \(error.localizedDescription)\n", stderr)
    exit(1)
}
cfg.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: diskAttachment)]

var fsDevices: [VZDirectorySharingDeviceConfiguration] = []
for s in shares {
    let sharedDir = VZSharedDirectory(url: URL(fileURLWithPath: s.path), readOnly: s.readOnly)
    let single = VZSingleDirectoryShare(directory: sharedDir)
    let dev = VZVirtioFileSystemDeviceConfiguration(tag: s.tag)
    dev.share = single
    fsDevices.append(dev)
}
cfg.directorySharingDevices = fsDevices
cfg.socketDevices = [VZVirtioSocketDeviceConfiguration()]

// Serial console pipe setup (hvc0).
let serialInPipe = Pipe()
let serialOutPipe = Pipe()
let serialCfg = VZVirtioConsoleDeviceSerialPortConfiguration()
serialCfg.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: serialInPipe.fileHandleForReading,
    fileHandleForWriting: serialOutPipe.fileHandleForWriting
)
cfg.serialPorts = [serialCfg]

do {
    try cfg.validate()
} catch {
    fputs("berth-vz: invalid VM configuration: \(error.localizedDescription)\n", stderr)
    exit(1)
}

let vmQueue = DispatchQueue(label: "berth.vz.vm")
let vm = VZVirtualMachine(configuration: cfg, queue: vmQueue)
let lifecycle = VMLifecycleDelegate()
vm.delegate = lifecycle

var retainedDelegates: [AnyObject] = []
let ptyDelegate: VsockPtyDelegate? = vsockPtyPort.map { _ in
    let isTty = (isatty(STDIN_FILENO) != 0) && (isatty(STDOUT_FILENO) != 0)
    return VsockPtyDelegate(isTty: isTty)
}

// Forward SIGWINCH and SIGTERM/SIGINT cleanly into guest PTY controller.
signal(SIGPIPE, SIG_IGN)
let winchSrc = DispatchSource.makeSignalSource(signal: SIGWINCH, queue: DispatchQueue.global())
signal(SIGWINCH, SIG_IGN)
winchSrc.setEventHandler { ptyDelegate?.sendResize() }
winchSrc.resume()

let termSrc = DispatchSource.makeSignalSource(signal: SIGTERM, queue: DispatchQueue.global())
signal(SIGTERM, SIG_IGN)
termSrc.setEventHandler {
    if let del = ptyDelegate {
        del.sendSignal(UInt8(SIGTERM))
        DispatchQueue.global().asyncAfter(deadline: .now() + 2.5) {
            restoreTermios()
            exit(143)
        }
    } else {
        restoreTermios()
        exit(143)
    }
}
termSrc.resume()

// Handle serial console stream (either setup automation or runtime console log).
if linuxSetup {
    let outHandle = serialOutPipe.fileHandleForReading
    let inHandle = serialInPipe.fileHandleForWriting
    let sPort = setupPort ?? ""
    let sTok = setupToken ?? ""
    let stateLock = NSLock()
    var triggeredSetup = false

    // Kernel boots directly into `init=/bin/sh` on hvc0 (zero login/password/firstboot).
    // Probe with split-quoted echo so serial echo never false-matches the real output.
    DispatchQueue.global(qos: .userInitiated).async {
        for _ in 0..<40 {
            Thread.sleep(forTimeInterval: 0.7)
            stateLock.lock()
            let done = triggeredSetup
            stateLock.unlock()
            if done { break }
            if let d = "echo BERTH_SH_\"\"READY\n".data(using: .utf8) {
                inHandle.write(d)
            }
        }
    }

    DispatchQueue.global(qos: .userInitiated).async {
        let stageUnit = """
stty -echo 2>/dev/null || true
mkdir -p /usr/local/sbin /etc/systemd/system/multi-user.target.wants
cat <<'BERTH_RUN_EOF' > /usr/local/sbin/berth-run-setup.sh
#!/bin/bash
set -euo pipefail
for i in $(seq 1 45); do
  GW=$(ip -4 route 2>/dev/null | awk '/default/{print $3; exit}')
  if [ -n "$GW" ]; then break; fi
  sleep 1
done
echo "[setup] host gateway=${GW:-missing}"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL -H "X-Setup-Token: \(sTok)" "http://$GW:\(sPort)/setup.sh" -o /tmp/berth_setup.sh
elif command -v python3 >/dev/null 2>&1; then
  python3 -c "import urllib.request; r=urllib.request.Request('http://$GW:\(sPort)/setup.sh', headers={'X-Setup-Token':'\(sTok)'}); open('/tmp/berth_setup.sh','wb').write(urllib.request.urlopen(r,timeout=30).read())"
else
  wget -qO /tmp/berth_setup.sh --header="X-Setup-Token: \(sTok)" "http://$GW:\(sPort)/setup.sh"
fi
exec bash /tmp/berth_setup.sh "http://$GW:\(sPort)" "\(sTok)"
BERTH_RUN_EOF
chmod 0755 /usr/local/sbin/berth-run-setup.sh
cat <<'BERTH_UNIT_EOF' > /etc/systemd/system/berth-setup.service
[Unit]
Description=Berth One-Time ARM64 VM Provisioner
After=network.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/berth-run-setup.sh
StandardOutput=journal+console
StandardError=journal+console
[Install]
WantedBy=multi-user.target
BERTH_UNIT_EOF
ln -sf /etc/systemd/system/berth-setup.service /etc/systemd/system/multi-user.target.wants/berth-setup.service
ln -sf /dev/null /etc/systemd/system/systemd-firstboot.service
sync
exec /usr/lib/systemd/systemd

"""
        var acc = ""
        while true {
            let chunk = outHandle.availableData
            if chunk.isEmpty { break }
            _ = writeAllData(fd: STDOUT_FILENO, data: chunk)
            if let s = String(data: chunk, encoding: .utf8) {
                acc += s
                if acc.count > 4096 { acc = String(acc.suffix(2048)) }
                stateLock.lock()
                let already = triggeredSetup
                if !already && acc.contains("BERTH_SH_READY") {
                    triggeredSetup = true
                    stateLock.unlock()
                    acc = ""
                    if let d = stageUnit.data(using: .utf8) { inHandle.write(d) }
                } else {
                    stateLock.unlock()
                }
            }
        }
    }
} else {
    let outHandle = serialOutPipe.fileHandleForReading
    let logFileHandle: FileHandle? = consoleLogPath.flatMap { path in
        FileManager.default.createFile(atPath: path, contents: nil)
        return FileHandle(forWritingAtPath: path)
    }
    DispatchQueue.global(qos: .utility).async {
        while true {
            let chunk = outHandle.availableData
            if chunk.isEmpty { break }
            logFileHandle?.write(chunk)
        }
        try? logFileHandle?.close()
    }
}

var finalExit: Int32 = 0
var hasExited = false
let exitLock = NSLock()

func finishAndExit(_ code: Int32) {
    exitLock.lock()
    if hasExited {
        exitLock.unlock()
        return
    }
    hasExited = true
    exitLock.unlock()
    restoreTermios()
    exit(code)
}

ptyDelegate?.onGuestExit = { code in
    finalExit = code
    // Give guest poweroff 1.5s to flush VirtioFS cleanly before hard-exiting with exact exit code.
    DispatchQueue.global().asyncAfter(deadline: .now() + 1.5) {
        finishAndExit(code)
    }
}

lifecycle.onStop = { err in
    if let e = err {
        fputs("berth-vz: VM stopped with error: \(e.localizedDescription)\n", stderr)
        finishAndExit(1)
        return
    }
    if let code = ptyDelegate?.guestExitCode {
        finishAndExit(code)
        return
    }
    if linuxSetup, let marker = setupMarkerPath {
        finishAndExit(FileManager.default.fileExists(atPath: marker) ? 0 : 1)
        return
    }
    if vsockPtyPort != nil {
        fputs("berth-vz: guest stopped before reporting command exit status\n", stderr)
        if let lp = consoleLogPath, let txt = try? String(contentsOfFile: lp, encoding: .utf8), !txt.isEmpty {
            fputs(txt, stderr)
        }
        finishAndExit(1)
        return
    }
    finishAndExit(finalExit)
}

vmQueue.async {
    if let vsockDev = vm.socketDevices.first as? VZVirtioSocketDevice {
        for fwd in vsockForwards {
            let del = VsockForwardDelegate(host: fwd.tcpHost, port: fwd.tcpPort)
            let listener = VZVirtioSocketListener()
            listener.delegate = del
            vsockDev.setSocketListener(listener, forPort: fwd.vsockPort)
            retainedDelegates.append(del)
            retainedDelegates.append(listener)
        }
        if let ptyPort = vsockPtyPort, let del = ptyDelegate {
            let listener = VZVirtioSocketListener()
            listener.delegate = del
            vsockDev.setSocketListener(listener, forPort: ptyPort)
            retainedDelegates.append(del)
            retainedDelegates.append(listener)
        }
    }
    vm.start { result in
        if case .failure(let err) = result {
            fputs("berth-vz: failed to start VM: \(err.localizedDescription)\n", stderr)
            finishAndExit(1)
        }
    }
}

// Hard safety timeout ONLY in setup mode (20 min); runtime sessions run as long as the user works.
if linuxSetup {
    DispatchQueue.global().asyncAfter(deadline: .now() + 1200) {
        fputs("berth-vz: setup timed out after 1200s\n", stderr)
        finishAndExit(124)
    }
}

dispatchMain()
"####;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::berth::{CliLaunchEnv, IsolationReport, LeaseCageProfile};
    use crate::sandbox::prepare_account_cage_home;
    use tempfile::TempDir;

    fn sample_spec(tmp: &Path) -> SandboxLaunchSpec {
        let cage = LeaseCageProfile {
            cage_id: "cage-25".into(),
            gate_epoch: 1,
            timezone: "America/Chicago".into(),
            country_code: "US".into(),
            locale: "en_US.UTF-8".into(),
            accept_language: "en-US,en;q=0.9".into(),
            asn_class: "residential_isp".into(),
            strict_mode: true,
        };
        let acct = prepare_account_cage_home(tmp, Some(25), &cage).unwrap();
        let mut set_env = BTreeMap::new();
        set_env.insert("AGS_BERTH_CAGE_TZ".into(), "America/Chicago".into());
        set_env.insert("TZ".into(), "America/Chicago".into());
        SandboxLaunchSpec {
            agent: "claude".into(),
            workspace_dir: tmp.join("project"),
            real_home: tmp.join("host_home"),
            state_dir: tmp.to_path_buf(),
            account_home: acct,
            cage,
            stock_bin: PathBuf::from("claude"),
            agent_args: vec!["-p".into(), "hello".into()],
            launch_env: CliLaunchEnv {
                agent: "claude".into(),
                isolation: IsolationReport {
                    mode: "vm".into(),
                    os_egress_isolation: true,
                    mount_namespace_localtime: true,
                    windows_interop_path_scrubbed: true,
                    strict_eligible: true,
                    strict_blockers: vec![],
                    host_visible_notes: vec![],
                },
                set_env,
                unset_env: vec!["OPENAI_API_KEY".into()],
                codex_shell_env_policy_toml: None,
                scrubbed_path: "/usr/bin:/bin".into(),
            },
            overlay_unset_env: vec![],
            overlay_set_env: BTreeMap::new(),
            overlay_fs_etc: None,
            allow_env_fallback: false,
            auto_rebind: true,
            use_vm: true,
        }
    }

    #[test]
    fn vm_launch_config_scrubs_berth_markers_and_pins_guest_home_and_proxies() {
        let tmp = TempDir::new().unwrap();
        let spec = sample_spec(tmp.path());
        let cfg = build_vm_launch_config(&spec, 41001, 41002, "ags-shim-secret", 501, 20);

        assert_eq!(cfg.command, vec!["claude", "-p", "hello"]);
        assert_eq!(cfg.timezone, "America/Chicago");
        assert_eq!(cfg.locale, "en_US.UTF-8");
        assert_eq!(cfg.uid, 501);
        assert_eq!(cfg.gid, 20);
        assert_eq!(cfg.env.get("HOME").unwrap(), "/home/dev");
        assert_eq!(cfg.env.get("USER").unwrap(), "dev");
        assert_eq!(cfg.env.get("ANTHROPIC_BASE_URL").unwrap(), "http://127.0.0.1:41001");
        assert_eq!(cfg.env.get("HTTP_PROXY").unwrap(), "http://bh:ags-shim-secret@127.0.0.1:41002");
        assert_eq!(cfg.env.get("ALL_PROXY").unwrap(), "socks5h://bh:ags-shim-secret@127.0.0.1:41002");
        assert!(!cfg.env.keys().any(|k| k.starts_with("AGS_BERTH_") || k.starts_with("BERTH_")));
    }

    #[test]
    fn vz_runtime_args_enforce_ro_disk_zero_nat_and_isolated_virtiofs_shares() {
        let args = build_vz_runtime_args(
            Path::new("/Users/alice/.local/state/berth/vm"),
            &MacosVmMeta::default(),
            Path::new("/Users/alice/code/demo"),
            Path::new("/Users/alice/.local/state/berth/cages/acct-25/home"),
            Path::new("/tmp/berth-vm-etc-1"),
            41001,
            41002,
            Path::new("/tmp/berth-vm-etc-1/console.log"),
        );

        assert!(args.contains(&"--disk-ro".to_string()));
        assert!(!args.contains(&"--linux-setup".to_string()));
        assert!(args.iter().any(|a| a.contains("rootflags=noload")));
        assert!(args.contains(&"workspace=/Users/alice/code/demo".to_string()));
        assert!(args.contains(&"cagehome=/Users/alice/.local/state/berth/cages/acct-25/home".to_string()));
        assert!(args.contains(&"cageetc=/tmp/berth-vm-etc-1:ro".to_string()));
        assert!(args.contains(&"5001=127.0.0.1:41001".to_string()));
        assert!(args.contains(&"5002=127.0.0.1:41002".to_string()));
        assert!(args.contains(&"5003".to_string()));
    }
}
