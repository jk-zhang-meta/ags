#!/usr/bin/env python3
"""Credential-free AGS environment contract/conformance harness.

The harness is deliberately conservative: it never invokes a provider API,
never uses a shell, never forwards the parent environment wholesale, and only
emits hashes/metadata for command output.  It is an observation test runner,
not an OS virtualizer.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


SCHEMA_VERSION = "ags-environment-contract-v1"
MAX_CAPTURE = 64 * 1024
SAFE_ENV_KEYS = {
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "PWD", "OLDPWD", "TERM",
    "TZ", "LANG", "LC_ALL", "LC_CTYPE", "LC_TIME", "LC_NUMERIC",
    "TMPDIR", "TMP", "TEMP",
}
VIRTUAL_COMMANDS = {
    "date", "env", "hostname", "id", "locale", "printenv", "pwd", "uname", "whoami",
}
NATIVE_COMMANDS = {
    "cargo", "git", "make", "node", "npm", "pip", "python", "python3", "ruby",
    "rustc", "go", "java", "clang", "gcc", "cmake", "pytest",
}
LEAK_COMMANDS = {
    "systemd-detect-virt", "dmesg", "hostnamectl",
}
LEAK_MARKERS = (
    "microsoft", "wsl", "drvfs", "9p", "docker", "podman", "kubernetes",
    "container", "qemu", "kvm", "hypervisor", "vmware", "virtualbox",
)
PROVIDER_NAMES = {
    "codex": ("codex", "codex-cli"),
    "claude": ("claude", "claude-code"),
    "gemini": ("gemini", "gemini-cli"),
}
NEGATIVE_PROBES: tuple[tuple[str, ...], ...] = (
    ("cat", "/proc/version"),
    ("cat", "/proc/sys/kernel/osrelease"),
    ("ls", "/mnt/c"),
    ("systemd-detect-virt",),
    ("hostnamectl", "--static"),
)


def emit(kind: str, **fields: Any) -> None:
    payload = {"schema_version": SCHEMA_VERSION, "kind": kind, **fields}
    print(json.dumps(payload, ensure_ascii=True, sort_keys=True), flush=True)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def safe_id(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    cleaned = re.sub(r"[^A-Za-z0-9_.:@/-]", "_", value[:128])
    return cleaned or None


def load_profile(path: str | None) -> dict[str, Any]:
    if not path:
        return {}
    try:
        source = sys.stdin if path == "-" else open(path, encoding="utf-8")
        with source:
            value = json.load(source)
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"profile unreadable: {type(exc).__name__}") from exc
    if not isinstance(value, dict):
        raise ValueError("profile must be a JSON object")
    return value


def profile_commands(profile: dict[str, Any]) -> dict[str, Any]:
    observations = profile.get("observations", {})
    if not isinstance(observations, dict):
        observations = {}
    value = profile.get("commands", observations.get("commands", {}))
    return value if isinstance(value, dict) else {}


def expected_command(profile: dict[str, Any], name: str) -> dict[str, Any] | None:
    value = profile_commands(profile).get(name)
    if isinstance(value, str):
        return {"status": "virtual", "stdout": value}
    return value if isinstance(value, dict) else None


def classify(argv: list[str]) -> tuple[str, str]:
    if not argv:
        return "unknown", "empty_command"
    if any(token in {";", "&&", "||", "|", ">", ">>", "<", "&"} for token in argv):
        return "unknown", "shell_operator"
    if any("$" in token or "`" in token for token in argv):
        return "unknown", "dynamic_expansion"
    name = Path(argv[0]).name
    if name in LEAK_COMMANDS or (name == "cat" and any("/proc/" in x for x in argv[1:])):
        return "host_visible", "carrier_probe"
    if name in VIRTUAL_COMMANDS:
        return "virtual", "profile_observation"
    if name in NATIVE_COMMANDS:
        return "native", "real_toolchain"
    return "unknown", "unclassified_command"


def parse_command(command: str) -> list[str]:
    try:
        argv = shlex.split(command, posix=True)
    except ValueError as exc:
        raise ValueError("command_parse_error") from exc
    if len(command) > 4096 or len(argv) > 64:
        raise ValueError("command_too_large")
    return argv


def child_env(profile: dict[str, Any]) -> dict[str, str]:
    configured = profile.get("env", profile.get("environment", {}))
    result: dict[str, str] = {}
    if isinstance(configured, dict):
        for key, value in configured.items():
            if isinstance(key, str) and key in SAFE_ENV_KEYS and isinstance(value, (str, int, float)):
                result[key] = str(value)
    result.setdefault("PATH", os.defpath)
    if "HOME" not in result:
        result["HOME"] = "/home/agent"
    if "USER" not in result:
        result["USER"] = "agent"
    result.setdefault("LANG", "C.UTF-8")
    return result


def run_argv(argv: list[str], profile: dict[str, Any], timeout: float) -> dict[str, Any]:
    """Run one argv without a shell and retain no output text."""
    env = child_env(profile)
    cwd = profile.get("cwd")
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        cwd = None
    started = time.monotonic()
    try:
        completed = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=max(0.1, min(timeout, 30.0)),
            check=False,
        )
        stdout = completed.stdout[:MAX_CAPTURE]
        stderr = completed.stderr[:MAX_CAPTURE]
        combined = (stdout + b"\n" + stderr).lower()
        marker_hits = sorted({marker for marker in LEAK_MARKERS if marker.encode() in combined})
        return {
            "exit_code": completed.returncode,
            "stdout_bytes": len(completed.stdout),
            "stderr_bytes": len(completed.stderr),
            "stdout_sha256": digest(stdout),
            "stderr_sha256": digest(stderr),
            "marker_hits": marker_hits,
            "timed_out": False,
            "duration_ms": int((time.monotonic() - started) * 1000),
        }
    except subprocess.TimeoutExpired:
        return {"exit_code": None, "timed_out": True, "duration_ms": int((time.monotonic() - started) * 1000)}
    except (OSError, ValueError) as exc:
        return {"exit_code": None, "timed_out": False, "error": type(exc).__name__}


def observed_status(declared: str, reason: str, result: dict[str, Any] | None,
                    expected: dict[str, Any] | None) -> str:
    if result is None:
        return "unknown"
    if result.get("error") or result.get("timed_out"):
        return "unknown"
    if reason == "carrier_probe":
        if result.get("exit_code") == 0:
            return "host_visible"
        # A denied/absent carrier is virtual only when the profile declares it.
        # Otherwise a non-zero result is inconclusive (missing command, policy,
        # or a genuine hidden path cannot be distinguished from this probe).
        return "virtual" if expected and expected.get("status") == "virtual" else "unknown"
    if declared == "virtual":
        if expected and "exit_code" in expected and expected["exit_code"] != result.get("exit_code"):
            return "host_visible"
        if not expected:
            return "host_visible"
        expected_hash = expected.get("stdout_sha256")
        if expected_hash is None and isinstance(expected.get("stdout"), str):
            expected_hash = digest(expected["stdout"].encode())
        return "virtual" if expected_hash and expected_hash == result.get("stdout_sha256") else "host_visible"
    if declared == "native":
        return "native"
    return "host_visible" if result.get("exit_code") == 0 else "unknown"


def command_evidence(command: str, profile: dict[str, Any], run: bool, timeout: float) -> dict[str, Any]:
    try:
        argv = parse_command(command)
    except ValueError as exc:
        return {"status": "unknown", "reason": str(exc), "command_sha256": digest(command.encode())}
    declared, reason = classify(argv)
    name = Path(argv[0]).name if argv else ""
    expected = expected_command(profile, name)
    result = run_argv(argv, profile, timeout) if run else None
    status = observed_status(declared, reason, result, expected)
    public_name = name if name in VIRTUAL_COMMANDS | NATIVE_COMMANDS | LEAK_COMMANDS | {"cat", "ls", "true", "false"} else "other"
    return {
        "status": status,
        "declared_status": declared,
        "reason": reason,
        "command_name": public_name,
        "command_sha256": digest(command.encode()),
        "executed": run,
        **({"result": result} if result is not None else {}),
    }


def discover(provider: str) -> None:
    names = PROVIDER_NAMES.get(provider, (provider,))
    found = next((name for name in names if shutil.which(name)), None)
    emit("discovery", provider=provider, executable=found, found=found is not None,
         invoked=False, network=False)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", help="profile JSON path, or - for stdin")
    parser.add_argument("--agent", choices=sorted(PROVIDER_NAMES), action="append")
    parser.add_argument("--command", help="one command to classify (never passed to a shell)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true", help="plan only (default)")
    mode.add_argument("--run", action="store_true", help="execute command and read-only leak probes")
    parser.add_argument("--timeout", type=float, default=5.0)
    args = parser.parse_args(argv)
    try:
        profile = load_profile(args.profile)
    except ValueError as exc:
        emit("error", error=str(exc))
        return 2
    run = bool(args.run)
    emit("header", profile_id=safe_id(profile.get("profile_id", profile.get("id"))),
         mode="run" if run else "dry-run", invoked_agent_api=False, network=False)
    for provider in args.agent or ():
        discover(provider)
    evidences: list[dict[str, Any]] = []
    if args.command:
        evidence = command_evidence(args.command, profile, run, args.timeout)
        emit("command", **evidence)
        evidences.append(evidence)
    # Leak probes are always planned; they execute only under explicit --run.
    for probe in NEGATIVE_PROBES:
        command = " ".join(shlex.quote(part) for part in probe)
        evidence = command_evidence(command, profile, run, args.timeout)
        evidence["probe"] = "negative_leak"
        emit("probe", **evidence)
        evidences.append(evidence)
    required = profile.get("required_virtual_commands", [])
    required = [x for x in required if isinstance(x, str)] if isinstance(required, list) else []
    allowed = profile.get("allow_host_visible", [])
    allowed_names = set(allowed) if isinstance(allowed, list) else set()
    blocked = any(
        item.get("status") == "host_visible" and item.get("command_name") not in allowed_names
        for item in evidences
    )
    missing = [name for name in required if not any(item.get("command_name") == name and item.get("status") == "virtual" for item in evidences)]
    unresolved = any(item.get("status") == "unknown" for item in evidences)
    summary_status = "blocked" if blocked else ("unknown" if missing or unresolved or not evidences else "equivalent")
    emit("summary", status=summary_status, required_missing=missing,
         host_visible_count=sum(item.get("status") == "host_visible" for item in evidences),
         unknown_count=sum(item.get("status") == "unknown" for item in evidences),
         evidence_count=len(evidences))
    return 1 if summary_status == "blocked" else (2 if summary_status == "unknown" else 0)


if __name__ == "__main__":
    raise SystemExit(main())
