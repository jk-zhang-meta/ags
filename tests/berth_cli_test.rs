//! End-to-end integration tests for `ags berth` CLI subcommands and
//! `plugins/ags/scripts/ags` Berth v2 integration & `codext` retirement.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

fn ags_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_berth"))
}

fn ags_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/ags/scripts/ags")
}

#[test]
fn cli_berth_init_status_clear_redacts_key_and_enforces_https() {
    let dir = TempDir::new().unwrap();
    let xdg_config = dir.path().join("config");
    let xdg_state = dir.path().join("state");

    // 1. Refuses plain http:// without --allow-insecure-http-for-tests.
    let bad = Command::new(ags_bin())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args([
            "berth",
            "init",
            "--url",
            "http://127.0.0.1:8080",
            "--key",
            "sk-berth-super-secret",
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success(), "http:// must be rejected by default");

    // 2. Succeeds with https:// and saves 0600 file.
    let ok = Command::new(ags_bin())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args([
            "--json",
            "berth",
            "init",
            "--url",
            "https://berth.example.com",
            "--key",
            "sk-berth-super-secret",
            "--mgmt-entry",
            "203.0.113.10:18443",
            "--zone",
            "cn_mainland",
            "--isolation",
            "tz-overlay",
        ])
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let init_stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(!init_stdout.contains("sk-berth-super-secret"));

    let cfg_path = xdg_config.join("berth/berth.json");
    assert!(cfg_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // 3. `ags berth status --json` redacts API key and reports host isolation capabilities.
    let status = Command::new(ags_bin())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["berth", "status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(!status_stdout.contains("sk-berth-super-secret"));
    let parsed: serde_json::Value = serde_json::from_str(&status_stdout).unwrap();
    assert_eq!(parsed["configured"], true);
    assert_eq!(parsed["config"]["zone_preference"], "cn_mainland");
    assert_eq!(parsed["isolation"]["mode"], "tz-overlay");

    // 4. `ags berth clear` removes config.
    let clear = Command::new(ags_bin())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["berth", "clear", "--json"])
        .output()
        .unwrap();
    assert!(clear.status.success());
    assert!(!cfg_path.exists());
}

#[test]
fn cli_berth_lease_and_launch_e2e_with_mock_central_ingress_and_stock_codex() {
    let dir = TempDir::new().unwrap();
    let xdg_config = dir.path().join("config");
    let xdg_state = dir.path().join("state");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(home.join(".codex/auth.json"), "{\"access_token\":\"stale-local\"}").unwrap();
    fs::write(home.join(".codex/config.toml"), "model = \"o3\"\n").unwrap();

    // Mock Ingress server answering /healthz and /v1/responses.
    let ingress = TcpListener::bind("127.0.0.1:0").unwrap();
    let ingress_addr = ingress.local_addr().unwrap();
    let saw_real_lease_token = Arc::new(AtomicBool::new(false));
    let saw_real_clone = Arc::clone(&saw_real_lease_token);
    let ingress_thread = thread::spawn(move || {
        // Expect 1 healthz probe (dry-run) + 1 healthz probe (real launch) + 1 /v1/responses call.
        for _ in 0..3 {
            let (mut s, _) = ingress.accept().unwrap();
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
            if cl > 0 {
                let mut body = vec![0u8; cl];
                reader.read_exact(&mut body).unwrap();
            }
            if req_line.starts_with("GET /healthz ") {
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .unwrap();
            } else if req_line.starts_with("POST /v1/responses ") {
                let lower = headers.to_ascii_lowercase();
                if lower.contains("authorization: bearer bst_real_live_lease_token")
                    && lower.contains("x-berth-cage-tz: asia/tokyo")
                    && lower.contains("x-berth-ags-version: 2")
                {
                    saw_real_clone.store(true, Ordering::SeqCst);
                }
                let body = r#"{"ok":true,"from":"berth-edge"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                s.write_all(resp.as_bytes()).unwrap();
            }
        }
    });

    // Mock Berth Central server answering 2 POST /api/v1/ags/leases + 2 DELETE /api/v1/ags/leases/lease_e2e.
    let central = TcpListener::bind("127.0.0.1:0").unwrap();
    let central_addr = central.local_addr().unwrap();
    let released_count = Arc::new(AtomicUsize::new(0));
    let released_clone = Arc::clone(&released_count);
    let central_thread = thread::spawn(move || {
        for _ in 0..4 {
            let (mut s, _) = central.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut req_line = String::new();
            reader.read_line(&mut req_line).unwrap();
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
            }
            if cl > 0 {
                let mut body = vec![0u8; cl];
                reader.read_exact(&mut body).unwrap();
            }
            if req_line.starts_with("POST /api/v1/ags/leases ") {
                let payload = serde_json::json!({
                    "lease_id": "lease_e2e",
                    "lease_token": "bst_real_live_lease_token",
                    "work_session_id": "ws_e2e",
                    "expires_at": "2026-12-01T00:00:00Z",
                    "cage": {
                        "cage_id": "cage_tokyo_e2e",
                        "gate_epoch": 1,
                        "timezone": "Asia/Tokyo",
                        "country_code": "JP",
                        "locale": "ja_JP.UTF-8",
                        "accept_language": "ja-JP,ja;q=0.9",
                        "asn_class": "residential",
                        "strict_mode": false
                    },
                    "zone": {
                        "zone_slug": "cn_mainland",
                        "ingress_candidates": [{
                            "node_id": "gz-1",
                            "name": "GZ Relay",
                            "url": format!("http://{ingress_addr}"),
                            "probe_url": format!("http://{ingress_addr}/healthz"),
                            "weight": 100
                        }]
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else if req_line.starts_with("DELETE /api/v1/ags/leases/lease_e2e ") {
                released_clone.fetch_add(1, Ordering::SeqCst);
                s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        }
    });

    // Configure Berth v2 pointing to mock Central.
    let init = Command::new(ags_bin())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args([
            "berth",
            "init",
            "--url",
            &format!("http://{central_addr}"),
            "--key",
            "sk-e2e",
            "--allow-insecure-http-for-tests",
        ])
        .output()
        .unwrap();
    assert!(init.status.success());

    // 1. Dry-run check (never prints real lease token).
    let dry = Command::new(ags_bin())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("AGENT_SESSION_STATE_DIR", &xdg_state)
        .args(["--json", "berth", "launch", "--agent", "codex", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "dry-run stderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_out = String::from_utf8_lossy(&dry.stdout);
    assert!(!dry_out.contains("bst_real_live_lease_token"));
    let dry_json: serde_json::Value = serde_json::from_str(&dry_out).unwrap();
    assert_eq!(dry_json["set_env"]["TZ"], "Asia/Tokyo");
    assert_eq!(dry_json["isolation"]["mode"], "tz-overlay");

    // 2. Real launch against a fake stock `codex` script that calls the loopback shim!
    let fake_bin_dir = dir.path().join("bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    let report_file = dir.path().join("child-report.txt");
    let fake_codex = fake_bin_dir.join("codex");
    fs::write(
        &fake_codex,
        r#"#!/usr/bin/env bash
set -euo pipefail
{
  echo "ARGS=$*"
  echo "TZ=${TZ:-}"
  echo "LANG=${LANG:-}"
  echo "LC_ALL=${LC_ALL:-}"
  echo "OPENAI_BASE_URL=${OPENAI_BASE_URL:-}"
  echo "OPENAI_API_KEY=${OPENAI_API_KEY:-}"
  echo "HTTPS_PROXY=${HTTPS_PROXY:-}"
  echo "NO_PROXY=${NO_PROXY:-}"
  echo "PATH=${PATH:-}"
  echo "CODEX_HOME=${CODEX_HOME:-}"
  echo "AUTH_EXISTS=$([ -e "${CODEX_HOME}/auth.json" ] && echo yes || echo no)"
  echo "CONFIG_HAS_POLICY=$(grep -c 'shell_environment_policy' "${CODEX_HOME}/config.toml" || true)"
} > "$BERTH_TEST_REPORT"
curl -fsS -X POST "${OPENAI_BASE_URL}/responses" \
  -H "Authorization: Bearer ${OPENAI_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"hello":"berth"}' >> "$BERTH_TEST_REPORT"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let launch = Command::new(ags_bin())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("AGENT_SESSION_STATE_DIR", &xdg_state)
        .env("AGENT_SESSION_CODEX_BINARY", &fake_codex)
        .env("BERTH_TEST_REPORT", &report_file)
        .env("HTTPS_PROXY", "http://10.0.0.1:8888")
        .env("NO_PROXY", "*")
        .env(
            "PATH",
            format!("/mnt/c/Windows/System32:{}", std::env::var("PATH").unwrap_or_default()),
        )
        .args([
            "berth",
            "launch",
            "--agent",
            "codex",
            "--",
            "--model",
            "o3",
        ])
        .output()
        .unwrap();
    assert!(
        launch.status.success(),
        "launch stderr: {}",
        String::from_utf8_lossy(&launch.stderr)
    );

    ingress_thread.join().unwrap();
    central_thread.join().unwrap();

    let report = fs::read_to_string(&report_file).unwrap();
    assert!(report.contains("ARGS=--model o3"), "report:\n{report}");
    assert!(report.contains("TZ=Asia/Tokyo"), "report:\n{report}");
    assert!(report.contains("LANG=ja_JP.UTF-8"), "report:\n{report}");
    assert!(report.contains("LC_ALL=ja_JP.UTF-8"), "report:\n{report}");
    assert!(report.contains("OPENAI_API_KEY=ags-shim-"), "report:\n{report}");
    assert!(!report.contains("bst_real_live_lease_token"), "report:\n{report}");
    assert!(report.contains("HTTPS_PROXY=\n"), "report:\n{report}");
    assert!(report.contains("NO_PROXY=\n"), "report:\n{report}");
    assert!(!report.contains("/mnt/c/"), "report:\n{report}");
    assert!(report.contains("AUTH_EXISTS=no"), "report:\n{report}");
    assert!(report.contains("CONFIG_HAS_POLICY=2"), "report:\n{report}");
    assert!(report.contains(r#""from":"berth-edge""#), "report:\n{report}");
    assert!(saw_real_lease_token.load(Ordering::SeqCst));
    assert_eq!(released_count.load(Ordering::SeqCst), 2);
}

#[test]
fn cli_berth_shim_reads_token_from_stdin_and_writes_0600_ready_file() {
    let dir = TempDir::new().unwrap();
    let ready_file = dir.path().join("shim-ready.json");

    let mut child = Command::new(ags_bin())
        .args([
            "berth",
            "shim",
            "--ingress-url",
            "http://127.0.0.1:9",
            "--lease-id",
            "lease_stdin_test",
            "--work-session-id",
            "ws_stdin",
            "--cage-tz",
            "Europe/London",
            "--cage-country",
            "GB",
            "--ready-file",
            ready_file.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"bst_stdin_secret_never_in_argv\n").unwrap();
    }

    // Wait up to 3s for ready_file to appear.
    let mut ready_raw = None;
    for _ in 0..60 {
        if ready_file.is_file() {
            ready_raw = Some(fs::read_to_string(&ready_file).unwrap());
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let ready_raw = ready_raw.expect("ready_file was not created by ags berth shim");
    assert!(!ready_raw.contains("bst_stdin_secret_never_in_argv"));
    let ready: serde_json::Value = serde_json::from_str(&ready_raw).unwrap();
    let listen_url = ready["listen_url"].as_str().unwrap();
    let shim_token = ready["shim_token"].as_str().unwrap();

    // Verify authenticated /healthz works on the running CLI shim process.
    let addr: SocketAddr = listen_url.strip_prefix("http://").unwrap().parse().unwrap();
    let mut s = TcpStream::connect(addr).unwrap();
    let req = format!(
        "GET /healthz HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {shim_token}\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 "), "got: {resp}");
    assert!(resp.contains("lease_stdin_test"));
    assert!(!resp.contains("bst_stdin_secret_never_in_argv"));

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn bash_wrapper_retires_codext_forwards_berth_and_honors_berth_cage_tz_without_ipwhois() {
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    let overlay_dir = dir.path().join("overlay");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&overlay_dir).unwrap();

    // Put `codext` alongside official `codex`, `claude`, `agy`, `grok` on PATH.
    for name in ["codext", "codex", "claude", "agy", "grok"] {
        let p = bin_dir.join(name);
        fs::write(&p, format!("#!/bin/sh\necho {name}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    // Trap `curl` so any attempt to contact ipwho.is fails loudly.
    let fake_curl = bin_dir.join("curl");
    fs::write(&fake_curl, "#!/bin/sh\necho CURL_CALLED >&2\nexit 99\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Minimal overlay profile.json to verify sync_overlay_timezone updates it.
    fs::write(
        overlay_dir.join("profile.json"),
        r#"{"clock":{"timezone":"UTC","offset_seconds":0},"locale":{"lang":"C","lc_all":"C","lc_time":"C"}}"#,
    )
    .unwrap();

    // 1. Test `plugins/ags/scripts/ags berth status --json` forwards cleanly to Rust binary!
    let berth_fwd = Command::new("bash")
        .arg(ags_script())
        .args(["berth", "status", "--json"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.path().join("cfg"))
        .env("XDG_STATE_HOME", &state)
        .env("AGS_CONVERTER_BINARY", ags_bin())
        .output()
        .unwrap();
    assert!(
        berth_fwd.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&berth_fwd.stderr)
    );
    let fwd_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&berth_fwd.stdout)).unwrap();
    assert_eq!(fwd_json["configured"], false);

    // 2. Test `resolve_agent_binary`, `reconcile_codex_binary_name`, `align_runtime_timezone`,
    //    and `usage`/`usage all` directly by sourcing the script's function definitions.
    let script_text = fs::read_to_string(ags_script()).unwrap();
    let split_marker = "# Forward `ags [global flags] berth";
    let defs_only = script_text
        .split(split_marker)
        .next()
        .expect("berth forward marker missing from plugins/ags/scripts/ags");
    let funcs_file = dir.path().join("ags-funcs.sh");
    fs::write(&funcs_file, defs_only).unwrap();

    // Also create a second bin dir containing ONLY `codext` (no `codex`) to verify
    // `reconcile_codex_binary_name` does NOT symlink `codext` as `codex`.
    let codext_only_dir = dir.path().join("codext-only");
    fs::create_dir_all(&codext_only_dir).unwrap();
    fs::copy(bin_dir.join("codext"), codext_only_dir.join("codext")).unwrap();

    let bash_check = Command::new("bash")
        .arg("-c")
        .arg(
            r#"
set -euo pipefail
source "$1"
printf 'CODEX=%s\n' "$(resolve_agent_binary codex)"
printf 'CLAUDE=%s\n' "$(resolve_agent_binary claude)"
printf 'AGY=%s\n' "$(resolve_agent_binary agy)"
printf 'GROK=%s\n' "$(resolve_agent_binary grok)"

# Ensure reconcile_codex_binary_name NEVER symlinks codext as codex.
PATH="$2:/usr/bin:/bin" reconcile_codex_binary_name maintain
PATH="$2:/usr/bin:/bin" reconcile_codex_binary_name install
printf 'SYMLINK_CREATED=%s\n' "$([ -e "$2/codex" ] && echo yes || echo no)"

# Ensure align_runtime_timezone uses AGS_BERTH_CAGE_TZ directly without calling curl/ipwho.is.
export AGENT_SESSION_DISABLE_GEO=0
export AGS_ENV_OVERLAY="$3"
export AGS_BERTH_CAGE_TZ="America/Chicago"
export AGS_BERTH_CAGE_COUNTRY="US"
export AGS_BERTH_CAGE_LOCALE="en_US.UTF-8"
align_runtime_timezone
printf 'TZ=%s\nLANG=%s\nLC_ALL=%s\nLC_TIME=%s\n' "${TZ:-}" "${LANG:-}" "${LC_ALL:-}" "${LC_TIME:-}"

# Ensure help text advertises `ags berth init` and retires `codext`.
help_all="$(usage all)"
printf 'HELP_HAS_BERTH=%s\n' "$(grep -Fq 'ags berth init' <<< "$help_all" && echo yes || echo no)"
printf 'HELP_HAS_CODEXT=%s\n' "$(grep -Fq 'codext' <<< "$help_all" && echo yes || echo no)"
"#,
        )
        .arg("ags-bash-unit")
        .arg(&funcs_file)
        .arg(&codext_only_dir)
        .arg(&overlay_dir)
        .env("HOME", &home)
        .env("AGENT_SESSION_STATE_DIR", &state)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
            ),
        )
        .output()
        .unwrap();

    assert!(
        bash_check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&bash_check.stderr)
    );
    let out = String::from_utf8_lossy(&bash_check.stdout);
    assert!(
        out.contains(&format!("CODEX={}", bin_dir.join("codex").display())),
        "out:\n{out}"
    );
    assert!(
        out.contains(&format!("CLAUDE={}", bin_dir.join("claude").display())),
        "out:\n{out}"
    );
    assert!(
        out.contains(&format!("AGY={}", bin_dir.join("agy").display())),
        "out:\n{out}"
    );
    assert!(
        out.contains(&format!("GROK={}", bin_dir.join("grok").display())),
        "out:\n{out}"
    );
    assert!(out.contains("SYMLINK_CREATED=no"), "out:\n{out}");
    assert!(out.contains("TZ=America/Chicago"), "out:\n{out}");
    assert!(out.contains("LANG=en_US.UTF-8"), "out:\n{out}");
    assert!(out.contains("LC_ALL=en_US.UTF-8"), "out:\n{out}");
    assert!(out.contains("LC_TIME=en_US.UTF-8"), "out:\n{out}");
    assert!(out.contains("HELP_HAS_BERTH=yes"), "out:\n{out}");
    assert!(out.contains("HELP_HAS_CODEXT=no"), "out:\n{out}");

    // Verify sync_overlay_timezone updated profile.json to America/Chicago.
    let profile: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(overlay_dir.join("profile.json")).unwrap())
            .unwrap();
    assert_eq!(profile["clock"]["timezone"], "America/Chicago");
    assert_eq!(profile["locale"]["lc_all"], "en_US.UTF-8");
}

#[test]
fn top_level_bh_symlink_init_status_codex_and_clear_work_without_berth_prefix() {
    let dir = TempDir::new().unwrap();
    let xdg_config = dir.path().join("config");
    let xdg_state = dir.path().join("state");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // Invoke via `bh` symlink pointing to `berth` binary.
    let bh_link = bin_dir.join("bh");
    #[cfg(unix)]
    std::os::unix::fs::symlink(ags_bin(), &bh_link).unwrap();
    #[cfg(not(unix))]
    let bh_link = ags_bin();

    // Mock Central + Ingress answering 1 dry-run lease + release.
    let ingress = TcpListener::bind("127.0.0.1:0").unwrap();
    let ingress_addr = ingress.local_addr().unwrap();
    let ing_thread = thread::spawn(move || {
        let (mut s, _) = ingress.accept().unwrap();
        let mut buf = [0u8; 512];
        let _ = s.read(&mut buf);
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });

    let central = TcpListener::bind("127.0.0.1:0").unwrap();
    let central_addr = central.local_addr().unwrap();
    let cen_thread = thread::spawn(move || {
        for _ in 0..2 {
            let (mut s, _) = central.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut req_line = String::new();
            reader.read_line(&mut req_line).unwrap();
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
            }
            if cl > 0 {
                let mut body = vec![0u8; cl];
                reader.read_exact(&mut body).unwrap();
            }
            if req_line.starts_with("POST /api/v1/ags/leases ") {
                let payload = serde_json::json!({
                    "lease_id": "lease_top_level",
                    "lease_token": "bst_top_level_token",
                    "work_session_id": "ws_top_level",
                    "expires_at": "2026-12-01T00:00:00Z",
                    "cage": {
                        "cage_id": "cage_top_1",
                        "gate_epoch": 1,
                        "timezone": "America/Chicago",
                        "country_code": "US",
                        "locale": "en_US.UTF-8",
                        "accept_language": "en-US,en;q=0.9",
                        "strict_mode": false
                    },
                    "zone": {
                        "zone_slug": "cn_mainland",
                        "ingress_candidates": [{
                            "node_id": "1",
                            "name": "po0-gz",
                            "url": format!("http://{ingress_addr}"),
                            "probe_url": format!("http://{ingress_addr}/healthz"),
                            "weight": 100
                        }]
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else if req_line.starts_with("DELETE /api/v1/ags/leases/lease_top_level ") {
                s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        }
    });

    // 1. `bh init --url ... --key ...` (top-level!)
    let init = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args([
            "init",
            "--url",
            &format!("http://{central_addr}"),
            "--key",
            "sk-top-level-secret",
            "--zone",
            "cn_mainland",
            "--allow-insecure-http-for-tests",
        ])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "bh init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(xdg_config.join("berth/berth.json").is_file());

    // 2. `bh status --json` (top-level!)
    let st = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(st.status.success());
    let st_json: serde_json::Value = serde_json::from_slice(&st.stdout).unwrap();
    assert_eq!(st_json["configured"], true);
    assert_eq!(st_json["config"]["zone_preference"], "cn_mainland");

    // 3. `bh codex --dry-run --json` (top-level!)
    let dry = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["codex", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "bh codex --dry-run failed: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["agent"], "codex");
    assert_eq!(dry_json["cage"]["timezone"], "America/Chicago");

    // 4. `bh clear` (top-level!)
    let clr = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["clear", "--json"])
        .output()
        .unwrap();
    assert!(clr.status.success());
    assert!(!xdg_config.join("berth/berth.json").exists());

    ing_thread.join().unwrap();
    cen_thread.join().unwrap();
}

#[test]
fn cli_bh_login_auto_selects_platform_api_key_for_codex_and_claude_and_logout() {
    let dir = TempDir::new().unwrap();
    let xdg_config = dir.path().join("config");
    let xdg_state = dir.path().join("state");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    let bh_link = bin_dir.join("bh");
    #[cfg(unix)]
    std::os::unix::fs::symlink(ags_bin(), &bh_link).unwrap();
    #[cfg(not(unix))]
    let bh_link = ags_bin();

    // Mock Ingress answering 4 dry-run probes.
    let ingress = TcpListener::bind("127.0.0.1:0").unwrap();
    let ingress_addr = ingress.local_addr().unwrap();
    let ing_thread = thread::spawn(move || {
        for _ in 0..4 {
            let (mut s, _) = ingress.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        }
    });

    // Mock Berth Central capturing exact Authorization headers sent to POST /api/v1/ags/leases.
    let lease_auth_headers = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let lease_auth_clone = Arc::clone(&lease_auth_headers);
    let saw_logout = Arc::new(AtomicBool::new(false));
    let saw_logout_clone = Arc::clone(&saw_logout);

    let central = TcpListener::bind("127.0.0.1:0").unwrap();
    let central_addr = central.local_addr().unwrap();
    let cen_thread = thread::spawn(move || {
        // Expected requests:
        // 1. POST /api/v1/auth/login
        // 2. GET /api/v1/keys (post-login summary)
        // 3. GET /api/v1/keys (`bh key list`)
        // 4. GET /api/v1/keys + POST /api/v1/ags/leases + DELETE (bh codex auto openai key)
        // 5. GET /api/v1/keys + POST /api/v1/ags/leases + DELETE (bh claude auto anthropic key)
        // 6. POST /api/v1/ags/leases + DELETE (bh codex with pinned key)
        // 7. POST /api/v1/ags/leases + DELETE (bh codex with --key oneshot)
        // 8. POST /api/v1/auth/logout
        for _ in 0..14 {
            let (mut s, _) = central.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut req_line = String::new();
            reader.read_line(&mut req_line).unwrap();
            let mut auth_header = String::new();
            let mut cl = 0usize;
            loop {
                let mut l = String::new();
                reader.read_line(&mut l).unwrap();
                if l.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = l.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        cl = v.trim().parse().unwrap_or(0);
                    } else if k.trim().eq_ignore_ascii_case("authorization") {
                        auth_header = v.trim().to_string();
                    }
                }
            }
            let mut body = vec![0u8; cl];
            if cl > 0 {
                reader.read_exact(&mut body).unwrap();
            }

            if req_line.starts_with("POST /api/v1/auth/login ") {
                let payload = serde_json::json!({
                    "code": 0,
                    "data": {
                        "access_token": "jwt_user_secret_token",
                        "refresh_token": "rt_user_secret_token",
                        "expires_in": 86400,
                        "token_type": "Bearer",
                        "user": { "id": 7, "email": "alice@example.com", "role": "user" }
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else if req_line.starts_with("GET /api/v1/keys") {
                assert_eq!(auth_header, "Bearer jwt_user_secret_token");
                let payload = serde_json::json!({
                    "code": 0,
                    "data": {
                        "items": [
                            {
                                "id": 101,
                                "name": "my-openai-key",
                                "key": "sk-openai-user-secret",
                                "status": "active",
                                "group_id": 1,
                                "group": { "id": 1, "name": "OpenAI", "platform": "openai", "status": "active" }
                            },
                            {
                                "id": 102,
                                "name": "my-anthropic-key",
                                "key": "sk-anthropic-user-secret",
                                "status": "active",
                                "group_id": 2,
                                "group": { "id": 2, "name": "Anthropic", "platform": "anthropic", "status": "active" }
                            }
                        ],
                        "total": 2
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else if req_line.starts_with("POST /api/v1/ags/leases ") {
                lease_auth_clone.lock().unwrap().push(auth_header);
                let payload = serde_json::json!({
                    "lease_id": "lease_login_e2e",
                    "lease_token": "bst_login_e2e",
                    "work_session_id": "ws_login_e2e",
                    "expires_at": "2026-12-01T00:00:00Z",
                    "cage": {
                        "cage_id": "cage_us_1",
                        "gate_epoch": 1,
                        "timezone": "America/Chicago",
                        "country_code": "US",
                        "locale": "en_US.UTF-8",
                        "accept_language": "en-US,en;q=0.9",
                        "strict_mode": false
                    },
                    "zone": {
                        "zone_slug": "overseas",
                        "ingress_candidates": [{
                            "node_id": "1",
                            "name": "hyonix-la",
                            "url": format!("http://{ingress_addr}"),
                            "probe_url": format!("http://{ingress_addr}/healthz"),
                            "weight": 100
                        }]
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else if req_line.starts_with("DELETE /api/v1/ags/leases/lease_login_e2e ") {
                s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            } else if req_line.starts_with("POST /api/v1/auth/logout ") {
                let body_str = String::from_utf8_lossy(&body);
                assert!(body_str.contains("rt_user_secret_token"), "logout body: {body_str}");
                saw_logout_clone.store(true, Ordering::SeqCst);
                let payload = r#"{"code":0,"data":{"message":"Logged out successfully"}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                s.write_all(resp.as_bytes()).unwrap();
            } else {
                panic!("unexpected central request: {req_line}");
            }
        }
    });

    // 1. `bh login --email alice@example.com --password-stdin`
    let mut login_child = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args([
            "login",
            "--json",
            "--url",
            &format!("http://{central_addr}"),
            "--email",
            "alice@example.com",
            "--password-stdin",
            "--allow-insecure-http-for-tests",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    login_child
        .stdin
        .take()
        .unwrap()
        .write_all(b"super-secret-pass\n")
        .unwrap();
    let login_out = login_child.wait_with_output().unwrap();
    assert!(
        login_out.status.success(),
        "bh login failed: {}",
        String::from_utf8_lossy(&login_out.stderr)
    );
    let login_stdout = String::from_utf8_lossy(&login_out.stdout);
    assert!(!login_stdout.contains("jwt_user_secret_token"));
    assert!(!login_stdout.contains("rt_user_secret_token"));
    assert!(!login_stdout.contains("sk-openai-user-secret"));
    assert!(!login_stdout.contains("sk-anthropic-user-secret"));
    let login_json: serde_json::Value = serde_json::from_str(&login_stdout).unwrap();
    assert_eq!(login_json["logged_in"], true);
    assert_eq!(login_json["user"]["email"], "alice@example.com");
    assert_eq!(login_json["remote_keys"].as_array().unwrap().len(), 2);

    // 2. `bh key list --json`
    let kl = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["key", "list", "--json"])
        .output()
        .unwrap();
    assert!(kl.status.success());
    let kl_stdout = String::from_utf8_lossy(&kl.stdout);
    assert!(!kl_stdout.contains("sk-openai-user-secret"));
    assert!(!kl_stdout.contains("sk-anthropic-user-secret"));

    // 3. `bh codex --dry-run --json` -> auto-selects openai key!
    let codex_dry = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["codex", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        codex_dry.status.success(),
        "codex dry-run stderr: {}",
        String::from_utf8_lossy(&codex_dry.stderr)
    );
    let codex_json: serde_json::Value = serde_json::from_slice(&codex_dry.stdout).unwrap();
    assert_eq!(codex_json["auth"]["source"], "auto-platform-key");
    assert_eq!(codex_json["auth"]["platform"], "openai");
    assert_eq!(codex_json["auth"]["key_name"], "my-openai-key");

    // 4. `bh claude --dry-run --json` -> auto-selects anthropic key!
    let claude_dry = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["claude", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(claude_dry.status.success());
    let claude_json: serde_json::Value = serde_json::from_slice(&claude_dry.stdout).unwrap();
    assert_eq!(claude_json["auth"]["source"], "auto-platform-key");
    assert_eq!(claude_json["auth"]["platform"], "anthropic");
    assert_eq!(claude_json["auth"]["key_name"], "my-anthropic-key");

    // 5. `bh key set codex sk-pinned-override` -> overrides auto-selection!
    let pin = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["key", "set", "codex", "sk-pinned-override", "--json"])
        .output()
        .unwrap();
    assert!(pin.status.success());
    let pinned_dry = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["codex", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(pinned_dry.status.success());
    let pinned_json: serde_json::Value = serde_json::from_slice(&pinned_dry.stdout).unwrap();
    assert_eq!(pinned_json["auth"]["source"], "pinned-platform-key");

    // 6. One-shot `--key sk-oneshot-override` beats pinned override!
    let oneshot_dry = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["codex", "--key", "sk-oneshot-override", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(oneshot_dry.status.success());
    let oneshot_json: serde_json::Value = serde_json::from_slice(&oneshot_dry.stdout).unwrap();
    assert_eq!(oneshot_json["auth"]["source"], "explicit-flag");

    // 7. `bh status --json` shows session email without leaking secrets.
    let status = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let st_out = String::from_utf8_lossy(&status.stdout);
    assert!(!st_out.contains("jwt_user_secret_token"));
    assert!(!st_out.contains("sk-pinned-override"));
    let st_json: serde_json::Value = serde_json::from_str(&st_out).unwrap();
    assert_eq!(st_json["config"]["user_session"]["email"], "alice@example.com");

    // 8. `bh key unset codex` + `bh logout --json` revokes refresh token and clears config!
    let unset = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["key", "unset", "codex", "--json"])
        .output()
        .unwrap();
    assert!(unset.status.success());

    let logout = Command::new(&bh_link)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["logout", "--json"])
        .output()
        .unwrap();
    assert!(logout.status.success());
    let logout_json: serde_json::Value = serde_json::from_slice(&logout.stdout).unwrap();
    assert_eq!(logout_json["logged_out"], true);
    assert_eq!(logout_json["revoked_remote"], true);
    assert_eq!(logout_json["config_removed"], true);
    assert!(!xdg_config.join("berth/berth.json").exists());

    ing_thread.join().unwrap();
    cen_thread.join().unwrap();
    assert!(saw_logout.load(Ordering::SeqCst));
    assert_eq!(
        *lease_auth_headers.lock().unwrap(),
        vec![
            "Bearer sk-openai-user-secret".to_string(),
            "Bearer sk-anthropic-user-secret".to_string(),
            "Bearer sk-pinned-override".to_string(),
            "Bearer sk-oneshot-override".to_string(),
        ]
    );
}
