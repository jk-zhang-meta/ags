#!/usr/bin/env python3
"""Collect non-secret environment facts for AGS conformance checks."""

from __future__ import annotations

import argparse
import datetime as dt
import getpass
import json
import locale
import os
import platform
import pwd
import re
import socket
import sys
import time
from pathlib import Path
from typing import Any


SCHEMA_VERSION = "ags-environment-probe-v1"
CARRIER_ENV_KEYS = (
    "WSL_DISTRO_NAME",
    "WSL_INTEROP",
    "WSLENV",
    "WSL2_GUI_APPS_ENABLED",
    "WT_SESSION",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "REMOTEHOST",
    "container",
    "CONTAINER",
    "KUBERNETES_SERVICE_HOST",
    "AWS_EXECUTION_ENV",
    "CODESPACES",
    "GITHUB_ACTIONS",
    "COMSPEC",
    "ComSpec",
    "SYSTEMROOT",
    "WINDIR",
    "ProgramFiles",
)


def read_text(path: str, limit: int = 262_144) -> str | None:
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read(limit)
    except OSError:
        return None


def os_release() -> dict[str, str]:
    content = read_text("/etc/os-release", 16_384)
    if content is None:
        return {}
    allowed = {"ID", "ID_LIKE", "NAME", "PRETTY_NAME", "VERSION_ID"}
    result: dict[str, str] = {}
    for line in content.splitlines():
        key, separator, value = line.partition("=")
        if separator and key in allowed:
            result[key.lower()] = value.strip().strip("\"'")
    return result


def uname_facts() -> dict[str, str | None]:
    try:
        value = os.uname()
        return {
            "sysname": value.sysname,
            "nodename": value.nodename,
            "release": value.release,
            "version": value.version,
            "machine": value.machine,
        }
    except AttributeError:
        return {
            "sysname": platform.system() or None,
            "nodename": platform.node() or None,
            "release": platform.release() or None,
            "version": platform.version() or None,
            "machine": platform.machine() or None,
        }


def timezone_name() -> tuple[str | None, str | None]:
    tz_env = os.environ.get("TZ")
    if tz_env:
        configured = tz_env.removeprefix(":")
        marker = "/zoneinfo/"
        if marker in configured:
            return configured.split(marker, 1)[1], "TZ"
        zoneinfo_roots = (Path("/usr/share/zoneinfo"), Path("/var/db/timezone/zoneinfo"))
        if configured in {"UTC", "GMT"} or any(
            (root / configured).is_file() for root in zoneinfo_roots
        ):
            return configured, "TZ"
        return None, "TZ"

    localtime = Path("/etc/localtime")
    try:
        target = str(localtime.resolve(strict=True))
        marker = "/zoneinfo/"
        if marker in target:
            return target.split(marker, 1)[1], "/etc/localtime"
    except OSError:
        pass

    configured = read_text("/etc/timezone", 256)
    if configured:
        name = configured.strip()
        if name and "\n" not in name:
            return name, "/etc/timezone"
    return None, None


def time_facts() -> dict[str, Any]:
    if hasattr(time, "tzset"):
        time.tzset()
    now = dt.datetime.now().astimezone()
    offset = now.utcoffset()
    iana_name, source = timezone_name()
    return {
        "current_utc": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
        "tz_environment": os.environ.get("TZ"),
        "iana_name": iana_name,
        "iana_source": source,
        "local_name": now.tzname(),
        "utc_offset_seconds": int(offset.total_seconds()) if offset is not None else None,
        "tzname": list(time.tzname),
        "daylight": bool(time.daylight),
    }


def locale_facts() -> dict[str, Any]:
    categories = {
        "all": locale.LC_ALL,
        "ctype": locale.LC_CTYPE,
        "numeric": locale.LC_NUMERIC,
        "time": locale.LC_TIME,
        "collate": locale.LC_COLLATE,
        "monetary": locale.LC_MONETARY,
    }
    active: dict[str, str | None] = {}
    for name, category in categories.items():
        try:
            active[name] = locale.setlocale(category, None)
        except locale.Error:
            active[name] = None
    return {
        "environment": {
            key: os.environ.get(key)
            for key in ("LANG", "LC_ALL", "LC_CTYPE", "LC_TIME")
        },
        "active": active,
        "preferred_encoding": locale.getpreferredencoding(False),
        "filesystem_encoding": sys.getfilesystemencoding(),
    }


def user_facts() -> dict[str, Any]:
    uid = os.getuid()
    try:
        entry = pwd.getpwuid(os.geteuid())
        name, passwd_home = entry.pw_name, entry.pw_dir
    except KeyError:
        name, passwd_home = None, None
    try:
        getpass_name = getpass.getuser()
    except OSError:
        getpass_name = None
    return {
        "name": name,
        "getpass_name": getpass_name,
        "uid": uid,
        "effective_uid": os.geteuid(),
        "gid": os.getgid(),
        "effective_gid": os.getegid(),
        "groups": sorted(os.getgroups()),
        "passwd_home": passwd_home,
    }


def stream_facts(stream: Any) -> dict[str, Any]:
    try:
        fd = stream.fileno()
    except (AttributeError, OSError):
        return {"isatty": False, "device": None, "columns": None, "rows": None}

    is_tty = os.isatty(fd)
    try:
        device = os.ttyname(fd) if is_tty else None
    except OSError:
        device = None
    try:
        size = os.get_terminal_size(fd)
        columns, rows = size.columns, size.lines
    except OSError:
        columns, rows = None, None
    return {"isatty": is_tty, "device": device, "columns": columns, "rows": rows}


def wsl_facts() -> dict[str, Any]:
    proc_version = read_text("/proc/version", 16_384)
    kernel_release = read_text("/proc/sys/kernel/osrelease", 4_096)
    mounts = read_text("/proc/self/mountinfo") or read_text("/proc/mounts")
    version_lower = (proc_version or "").lower()
    release_lower = (kernel_release or "").lower()
    mount_lower = (mounts or "").lower()
    env_markers = {
        key: key in os.environ for key in ("WSL_DISTRO_NAME", "WSL_INTEROP", "WSLENV")
    }
    markers = {
        "proc_version_microsoft": "microsoft" in version_lower,
        "proc_version_wsl": "wsl" in version_lower,
        "kernel_release_microsoft": "microsoft" in release_lower,
        "kernel_release_wsl": "wsl" in release_lower,
        "mount_drvfs": "drvfs" in mount_lower,
        "mount_9p": " - 9p " in mount_lower or " type 9p " in mount_lower,
        "mnt_c_exists": Path("/mnt/c").exists(),
        "mnt_wsl_exists": Path("/mnt/wsl").exists(),
        "mnt_wslg_exists": Path("/mnt/wslg").exists(),
        "wsl_init_exists": Path("/init").exists(),
        "wsl_lib_exists": Path("/usr/lib/wsl").exists(),
    }
    kernel_marker = any(
        markers[key]
        for key in (
            "proc_version_microsoft",
            "proc_version_wsl",
            "kernel_release_microsoft",
            "kernel_release_wsl",
        )
    )
    return {
        "detected": (
            any(env_markers.values())
            or kernel_marker
            or (markers["wsl_lib_exists"] and markers["mnt_wsl_exists"])
        ),
        "environment_markers": env_markers,
        "system_markers": markers,
    }


def proc_facts() -> dict[str, Any]:
    available = Path("/proc/self").is_dir()
    pid1 = read_text("/proc/1/comm", 256) if available else None
    return {
        "available": available,
        "pid": os.getpid(),
        "parent_pid": os.getppid(),
        "pid1_name": pid1.strip() if pid1 else None,
    }


def collect_facts() -> dict[str, Any]:
    try:
        home = str(Path.home())
    except RuntimeError:
        home = None
    path_value = os.environ.get("PATH", "")
    path_markers = {
        "contains_windows_drive": bool(re.search(r"(?:^|[:;])[A-Za-z]:[\\/]", path_value)),
        "contains_mnt_c": "/mnt/c" in path_value.lower(),
        "contains_wsl_unc": "\\\\wsl" in path_value.lower(),
    }
    return {
        "os": {
            "system": platform.system(),
            "release": platform.release(),
            "version": platform.version(),
            "machine": platform.machine(),
            "mac_version": platform.mac_ver()[0] or None,
            "os_release": os_release(),
        },
        "uname": uname_facts(),
        "hostname": socket.gethostname(),
        "paths": {
            "cwd": os.getcwd(),
            "pwd_environment": os.environ.get("PWD"),
            "home": home,
            "home_environment": os.environ.get("HOME"),
        },
        "user": user_facts(),
        "time": time_facts(),
        "locale": locale_facts(),
        "environment": {
            "carrier_markers": {key: key in os.environ for key in CARRIER_ENV_KEYS},
            "path_markers": path_markers,
        },
        "proc": proc_facts(),
        "wsl": wsl_facts(),
        "pty": {
            "stdin": stream_facts(sys.stdin),
            "stdout": stream_facts(sys.stdout),
            "stderr": stream_facts(sys.stderr),
            "term": os.environ.get("TERM"),
            "colorterm": os.environ.get("COLORTERM"),
        },
    }


def compare_subset(expected: Any, actual: Any, path: str = "$") -> list[dict[str, str]]:
    mismatches: list[dict[str, str]] = []
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return [{"path": path, "reason": "type_mismatch"}]
        for key, value in expected.items():
            child_path = f"{path}.{key}"
            if key not in actual:
                mismatches.append({"path": child_path, "reason": "missing"})
            else:
                mismatches.extend(compare_subset(value, actual[key], child_path))
    elif type(expected) is not type(actual):
        mismatches.append({"path": path, "reason": "type_mismatch"})
    elif expected != actual:
        mismatches.append({"path": path, "reason": "value_mismatch"})
    return mismatches


def load_expected(source: str) -> dict[str, Any]:
    if source == "-":
        value = json.load(sys.stdin)
    else:
        with open(source, encoding="utf-8") as handle:
            value = json.load(handle)
    if not isinstance(value, dict):
        raise ValueError("expected profile must be a JSON object")
    return value


def emit(value: dict[str, Any]) -> None:
    json.dump(value, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--expected",
        metavar="PATH|-",
        help="compare facts with a partial expected-profile JSON file (or stdin)",
    )
    args = parser.parse_args()

    try:
        facts = collect_facts()
        expected = load_expected(args.expected) if args.expected else None
    except Exception as error:
        emit({"schema_version": SCHEMA_VERSION, "ok": False, "error": type(error).__name__})
        return 2

    mismatches = compare_subset(expected, facts) if expected is not None else []
    emit(
        {
            "schema_version": SCHEMA_VERSION,
            "ok": not mismatches,
            "facts": facts,
            "comparison": (
                {"matched": not mismatches, "mismatches": mismatches}
                if expected is not None
                else None
            ),
        }
    )
    return 1 if mismatches else 0


if __name__ == "__main__":
    raise SystemExit(main())
