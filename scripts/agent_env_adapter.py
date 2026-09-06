#!/usr/bin/env python3
"""Provider-neutral AGS environment adapter.

This module is the small protocol boundary used by the Claude Code and Gemini
CLI command hooks.  It is intentionally dependency-free and conservative:

* input is parsed as JSON or an argv vector; no input is ever passed through a
  shell parser and then executed;
* only exact, profile-backed observations are rewritten;
* an unknown or carrier-visible command is denied unless the profile explicitly
  names it in ``policy.allowed_host_visible``;
* values returned to a provider are generated from the profile, never from the
  host environment;
* PostToolUse/AfterTool rewriting is supported for compatibility, but the
  adapter reports that the command has already executed.  Only a pre-tool hook
  can prevent side effects.

The profile format is ``environment-contract-v1`` as defined by AGS.  A few
optional JSON aliases (``environment``/``env`` and ``allow_host_visible``) are
accepted so the adapter can be used while a profile is being migrated.
"""

from __future__ import annotations

import argparse
import datetime as _datetime
import hashlib
import json
import locale as _py_locale
import math
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import time
from typing import Any, Iterable


SCHEMA_VERSION = "ags-agent-env-adapter-v1"
PROFILE_CONTRACT = "environment-contract-v1"
MAX_INPUT = 1_048_576
MAX_COMMAND = 8_192
MAX_OUTPUT = 128 * 1024
PROFILE_FD = 198

SAFE_ENV_KEYS = {
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "PWD",
    "OLDPWD",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TIME",
    "LC_NUMERIC",
    "TMPDIR",
    "TMP",
    "TEMP",
}

VIRTUAL_COMMANDS = {"date", "env", "hostname", "id", "locale", "printenv", "pwd", "uname", "whoami"}
SHELL_NAMES = {"bash", "dash", "fish", "ksh", "pwsh", "powershell", "sh", "zsh"}
SHELL_OPERATORS = {";", "&&", "||", "|", ">", ">>", "<", "<<", "&", "\n", "\r"}
COMMAND_FIELDS = ("command", "cmd", "shell_command", "shellCommand")
TOOL_FIELDS = ("tool_name", "toolName", "name")
SHELL_TOOLS = {"bash", "shell", "run_shell_command", "execute_command"}

# These markers are deliberately conservative.  They are only used to block
# paths that are known to identify the carrier; they are never emitted.
DEFAULT_CARRIER_MARKERS = (
    "/mnt/c",
    "/mnt/",
    "/proc/version",
    "/proc/sys/kernel",
    "/sys/hypervisor",
    "microsoft",
    "wsl",
    "drvfs",
    "docker",
    "podman",
    "container",
    "kvm",
    "qemu",
    "vmware",
    "virtualbox",
)

# A command in this set exposes host/kernel/hypervisor facts even when the
# caller has explicitly enabled the broad ``*`` host-visible allowlist.  The
# adapter cannot translate those facts, so allowing them would turn an
# explicit convenience switch into a carrier-identification escape hatch.
CARRIER_PROBE_COMMANDS = {
    "acpi",
    "cloud-init",
    "dmidecode",
    "dmesg",
    "findmnt",
    "hostnamectl",
    "ioreg",
    "ip",
    "ifconfig",
    "lscpu",
    "lsblk",
    "lshw",
    "mount",
    "route",
    "sw_vers",
    "sysctl",
    "system_profiler",
    "systemd-detect-virt",
    "virt-what",
    "wslpath",
}

# Interpreters can perform arbitrary OS observations even when their command
# line contains no obvious carrier marker.  Treating them as host-visible
# under a wildcard would make `python3 -c ...` an easy way around the profile
# gate.  They remain available only when a deployment adds a precise,
# separately audited rule (the wildcard never overrides this set).
CARRIER_PROBE_INTERPRETERS = {
    "awk",
    "bun",
    "deno",
    "node",
    "perl",
    "php",
    "python",
    "python2",
    "python3",
    "ruby",
}

# Path fragments that are useful to a normal diagnostic command but cannot be
# made target-equivalent by an environment variable rewrite.  Keep these
# separate from ``DEFAULT_CARRIER_MARKERS`` so a profile can still add its own
# precise markers without changing the command policy.
CARRIER_PROBE_PATH_MARKERS = (
    "/proc/",
    "/sys/",
    "/dev/dmi",
    "/dev/kvm",
    "/etc/os-release",
    "/etc/machine-id",
    "/etc/hostname",
    "/etc/localtime",
    "/run/host",
    "/run/wsl",
    "/usr/lib/wsl",
    "169.254.169.254",
    "powershell",
    "cmd.exe",
    "wsl_interop",
)

MAX_UINT32 = (1 << 32) - 1
MAX_INT64 = (1 << 63) - 1
MIN_INT64 = -(1 << 63)


class AdapterError(ValueError):
    """A malformed request or profile that must fail closed."""


def _sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8", "surrogatepass")).hexdigest()


def _safe_name(value: Any) -> str:
    if not isinstance(value, str):
        return "unknown"
    return value[:80]


def _json_line(value: dict[str, Any]) -> str:
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"))


def _read_profile(path: str | None) -> dict[str, Any]:
    selected = path or _inherited_profile_fd() or os.environ.get("AGS_ENV_PROFILE")
    if not selected:
        raise AdapterError("AGS_ENV_PROFILE or --profile is required")
    try:
        source = sys.stdin if selected == "-" else open(selected, encoding="utf-8")
        with source:
            raw = source.read(MAX_INPUT + 1)
    except OSError as exc:
        raise AdapterError(f"profile unreadable: {type(exc).__name__}") from exc
    if len(raw.encode("utf-8", "surrogatepass")) > MAX_INPUT:
        raise AdapterError("profile_too_large")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise AdapterError("profile_invalid_json") from exc
    if not isinstance(value, dict):
        raise AdapterError("profile_must_be_object")
    contract = value.get("contract")
    if contract not in (None, PROFILE_CONTRACT):
        raise AdapterError("profile_contract_mismatch")
    try:
        _validate_profile(value)
    except AdapterError:
        raise
    except (TypeError, ValueError, OverflowError) as exc:
        # A profile is untrusted input (it may have been generated on another
        # host).  Never let a conversion error escape a hook invocation and
        # leave the provider waiting for JSON.
        raise AdapterError("profile_invalid") from exc
    return value


def _inherited_profile_fd() -> str | None:
    """Locate the wrapper's profile descriptor without an environment key.

    `run_exec` passes the descriptor at a fixed number to the provider and its
    descendants.  The shell shim can therefore load the exact profile while
    the provider's model-visible environment contains no AGS control variable.
    This is a POSIX-only seam; on another platform callers must pass
    ``--profile`` explicitly.
    """
    if os.name != "posix":
        return None
    for candidate in (f"/proc/self/fd/{PROFILE_FD}", f"/dev/fd/{PROFILE_FD}"):
        if os.path.exists(candidate):
            return candidate
    return None


def _section(profile: dict[str, Any], name: str) -> dict[str, Any]:
    value = profile.get(name, {})
    return value if isinstance(value, dict) else {}


def _required_string(profile: dict[str, Any], section: str, field: str, default: str | None = None) -> str:
    value = _section(profile, section).get(field, default)
    if not isinstance(value, str) or not value or any(c in value for c in "\0\r\n"):
        raise AdapterError(f"profile_{section}_{field}_invalid")
    return value


def _required_uint(profile: dict[str, Any], section: str, field: str, maximum: int) -> int:
    """Validate an integer scalar without accepting bool/float coercions."""
    value = _section(profile, section).get(field)
    # ``bool`` is an ``int`` subclass in Python, but accepting it here would
    # silently turn a malformed profile into uid=1/gid=0.
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= maximum:
        raise AdapterError(f"profile_{section}_{field}_invalid")
    return value


def _optional_scalar_string(value: Any, field: str) -> None:
    if value is not None and (
        not isinstance(value, str)
        or not value
        or any(character in value for character in "\0\r\n")
    ):
        raise AdapterError(f"profile_{field}_invalid")


def _validate_profile(profile: dict[str, Any]) -> None:
    # The adapter accepts a short profile for direct command tests, but once a
    # target/identity section is present every value used in the virtual view
    # must be a valid scalar.  This avoids accidentally falling back to host
    # identity when a profile is partially written.
    for section, field in (
        ("identity", "hostname"),
        ("identity", "username"),
        ("paths", "cwd"),
        ("paths", "home"),
        ("paths", "tmp"),
        ("clock", "timezone"),
        ("locale", "lang"),
        ("locale", "lc_all"),
    ):
        _required_string(profile, section, field)
    _required_uint(profile, "identity", "uid", MAX_UINT32)
    _required_uint(profile, "identity", "gid", MAX_UINT32)

    identity = _section(profile, "identity")
    groups = identity.get("groups", [])
    if not isinstance(groups, list) or any(
        not isinstance(group, (str, int))
        or isinstance(group, bool)
        or (isinstance(group, str) and (not group or any(c in group for c in "\0\r\n")))
        for group in groups
    ):
        raise AdapterError("profile_identity_groups_invalid")
    _optional_scalar_string(identity.get("shell"), "identity_shell")

    paths = _section(profile, "paths")
    for field in ("cwd", "home", "tmp", "workspace", "proc", "sys", "mounts", "self_exe"):
        if field in paths:
            _optional_scalar_string(paths.get(field), f"paths_{field}")
    markers = paths.get("hidden_host_markers", profile.get("hidden_host_markers", []))
    if markers is not None and (
        not isinstance(markers, list)
        or any(
            not isinstance(marker, str)
            or not marker
            or any(c in marker for c in "\0\r\n")
            for marker in markers
        )
    ):
        raise AdapterError("profile_hidden_host_markers_invalid")

    locale = _section(profile, "locale")
    for field in ("lc_time", "charset", "icu_version"):
        if field in locale:
            _optional_scalar_string(locale.get(field), f"locale_{field}")

    configured_environment = profile.get("environment", profile.get("env", {}))
    if configured_environment is not None:
        if not isinstance(configured_environment, dict):
            raise AdapterError("profile_environment_invalid")
        for key, value in configured_environment.items():
            if not isinstance(key, str) or key not in SAFE_ENV_KEYS:
                raise AdapterError("profile_environment_key_invalid")
            if isinstance(value, bool) or not isinstance(value, (str, int, float)):
                raise AdapterError("profile_environment_value_invalid")
            if isinstance(value, float) and not math.isfinite(value):
                raise AdapterError("profile_environment_value_invalid")
            if isinstance(value, str) and any(c in value for c in "\0\r\n"):
                raise AdapterError("profile_environment_value_invalid")

    target = _section(profile, "target")
    if target and target.get("os") not in ("linux", "macos"):
        raise AdapterError("profile_target_os_invalid")
    for field in ("distribution", "version", "architecture", "abi"):
        if field in target:
            _optional_scalar_string(target.get(field), f"target_{field}")
    clock = _section(profile, "clock")
    anchor = clock.get("anchor_epoch_seconds", 0)
    if isinstance(anchor, bool) or not isinstance(anchor, int) or not MIN_INT64 <= anchor <= MAX_INT64:
        raise AdapterError("profile_clock_anchor_invalid")
    rate = clock.get("rate", 1.0)
    if isinstance(rate, bool) or not isinstance(rate, (int, float)) or not math.isfinite(float(rate)) or rate < 0:
        raise AdapterError("profile_clock_rate_invalid")
    offset = clock.get("offset_seconds")
    if offset is not None and (
        isinstance(offset, bool)
        or not isinstance(offset, (int, float))
        or not math.isfinite(float(offset))
        or int(offset) != offset
        or not -86_400 <= int(offset) <= 86_400
    ):
        raise AdapterError("profile_clock_offset_invalid")
    # A non-unit rate needs an explicit real-time anchor.  Without one there
    # is no deterministic way to know how much virtual time has elapsed, so a
    # date response would otherwise depend on an undocumented host clock.
    real_anchor = clock.get("real_anchor_epoch_seconds")
    if real_anchor is not None and (
        isinstance(real_anchor, bool)
        or not isinstance(real_anchor, (int, float))
        or not math.isfinite(float(real_anchor))
    ):
        raise AdapterError("profile_clock_real_anchor_invalid")
    if float(rate) not in (0.0, 1.0) and real_anchor is None:
        raise AdapterError("profile_clock_rate_requires_real_anchor")
    follow_realtime = clock.get("follow_realtime", False)
    if follow_realtime is not True and follow_realtime is not False:
        raise AdapterError("profile_clock_follow_realtime_invalid")


def _target_os(profile: dict[str, Any]) -> str:
    value = _section(profile, "target").get("os", "linux")
    return value if value in ("linux", "macos") else "linux"


def _identity(profile: dict[str, Any]) -> dict[str, Any]:
    section = _section(profile, "identity")
    hostname = _required_string(profile, "identity", "hostname", "ags")
    username = _required_string(profile, "identity", "username", "agent")
    uid = _required_uint(profile, "identity", "uid", MAX_UINT32)
    gid = _required_uint(profile, "identity", "gid", MAX_UINT32)
    groups = section.get("groups", [])
    if not isinstance(groups, list):
        raise AdapterError("profile_identity_groups_invalid")
    normalized_groups: list[str] = []
    for group in groups:
        if isinstance(group, bool) or not isinstance(group, (str, int)):
            raise AdapterError("profile_identity_groups_invalid")
        if isinstance(group, str) and (not group or any(c in group for c in "\0\r\n")):
            raise AdapterError("profile_identity_groups_invalid")
        normalized_groups.append(str(group))
    shell = section.get("shell")
    if shell is not None and (not isinstance(shell, str) or not shell or any(c in shell for c in "\0\r\n")):
        raise AdapterError("profile_identity_shell_invalid")
    return {
        "hostname": hostname,
        "username": username,
        "uid": uid,
        "gid": gid,
        "groups": normalized_groups,
        "shell": shell,
    }


def _paths(profile: dict[str, Any]) -> dict[str, str]:
    section = _section(profile, "paths")
    return {
        "cwd": _required_string(profile, "paths", "cwd", "/workspace"),
        "home": _required_string(profile, "paths", "home", "/home/agent"),
        "tmp": _required_string(profile, "paths", "tmp", "/tmp"),
    }


def _clock(profile: dict[str, Any]) -> dict[str, Any]:
    section = _section(profile, "clock")
    timezone = _required_string(profile, "clock", "timezone", "UTC")
    anchor = section.get("anchor_epoch_seconds", 0)
    if isinstance(anchor, bool) or not isinstance(anchor, int) or not MIN_INT64 <= anchor <= MAX_INT64:
        raise AdapterError("profile_clock_anchor_invalid")
    rate = section.get("rate", 1.0)
    if isinstance(rate, bool) or not isinstance(rate, (int, float)) or not math.isfinite(float(rate)) or rate < 0:
        raise AdapterError("profile_clock_rate_invalid")
    offset = section.get("offset_seconds")
    real_anchor = section.get("real_anchor_epoch_seconds")
    if real_anchor is not None and (
        isinstance(real_anchor, bool)
        or not isinstance(real_anchor, (int, float))
        or not math.isfinite(float(real_anchor))
    ):
        raise AdapterError("profile_clock_real_anchor_invalid")
    if float(rate) != 1.0 and real_anchor is None:
        raise AdapterError("profile_clock_rate_requires_real_anchor")
    follow_realtime = section.get("follow_realtime", False)
    if follow_realtime is not True and follow_realtime is not False:
        raise AdapterError("profile_clock_follow_realtime_invalid")
    return {
        "timezone": timezone,
        "anchor": anchor,
        "rate": float(rate),
        # This optional extension is useful when the profile has a pinned tzdb
        # compiler.  It is never inferred from the host timezone database.
        "offset_seconds": offset,
        "real_anchor": real_anchor,
        "follow_realtime": bool(follow_realtime),
    }


def _locale(profile: dict[str, Any]) -> dict[str, str]:
    section = _section(profile, "locale")
    lang = _required_string(profile, "locale", "lang", "C.UTF-8")
    lc_all = _required_string(profile, "locale", "lc_all", lang)
    lc_time = section.get("lc_time", lc_all)
    if not isinstance(lc_time, str) or not lc_time or any(c in lc_time for c in "\0\r\n"):
        raise AdapterError("profile_locale_lc_time_invalid")
    return {
        "lang": lang,
        "lc_all": lc_all,
        "lc_time": lc_time,
    }


def virtual_env(profile: dict[str, Any], inherited: Iterable[tuple[str, str]] = ()) -> dict[str, str]:
    """Return a profile-only environment with harmless terminal hints."""
    result: dict[str, str] = {}
    for key, value in inherited:
        if key in {"TERM", "COLORTERM", "NO_COLOR"}:
            result[key] = value
    identity = _identity(profile)
    paths = _paths(profile)
    locale = _locale(profile)
    clock = _clock(profile)
    target_os = _target_os(profile)
    result.update(
        {
            "HOME": paths["home"],
            "PWD": paths["cwd"],
            "OLDPWD": paths["cwd"],
            "TMPDIR": paths["tmp"],
            "USER": identity["username"],
            "LOGNAME": identity["username"],
            "HOSTNAME": identity["hostname"],
            "LANG": locale["lang"],
            "LC_ALL": locale["lc_all"],
            "LC_TIME": locale["lc_time"],
            "TZ": clock["timezone"],
            "PATH": "/usr/local/bin:/usr/bin:/bin" if target_os == "linux" else "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHELL": identity["shell"] or ("/bin/bash" if target_os == "linux" else "/bin/zsh"),
        }
    )
    configured = profile.get("environment", profile.get("env", {}))
    if isinstance(configured, dict):
        for key, value in configured.items():
            if isinstance(key, str) and key in SAFE_ENV_KEYS and isinstance(value, (str, int, float)):
                result[key] = str(value)
    # Profile values always win over optional aliases.
    result.update(
        {
            "HOME": paths["home"],
            "PWD": paths["cwd"],
            "OLDPWD": paths["cwd"],
            "TMPDIR": paths["tmp"],
            "USER": identity["username"],
            "LOGNAME": identity["username"],
            "HOSTNAME": identity["hostname"],
            "LANG": locale["lang"],
            "LC_ALL": locale["lc_all"],
            "LC_TIME": locale["lc_time"],
            "TZ": clock["timezone"],
            "PATH": "/usr/local/bin:/usr/bin:/bin" if target_os == "linux" else "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHELL": identity["shell"] or ("/bin/bash" if target_os == "linux" else "/bin/zsh"),
        }
    )
    return dict(sorted(result.items()))


def _basename(value: str) -> str:
    return Path(value).name or value


def _has_unquoted_shell_operator(command: str) -> bool:
    """Return whether a shell metacharacter occurs outside quotes.

    Provider Bash tools execute a command *string* in a shell.  ``shlex``
    alone is not enough for the gate because it treats an unquoted newline as
    whitespace, which could turn ``cmd\nuntrusted`` into two shell commands.
    Quoted data (including the newline in an AGS-generated ``printf`` payload)
    is safe and is therefore ignored by this scanner.
    """
    quote: str | None = None
    escaped = False
    for character in command:
        if escaped:
            escaped = False
            continue
        if character == "\\":
            escaped = True
            continue
        if quote is not None:
            if character == quote:
                quote = None
            continue
        if character in {"'", '"'}:
            quote = character
        elif character in SHELL_OPERATORS:
            return True
    return False


def _has_unquoted_dynamic_expansion(command: str) -> bool:
    """Return whether ``$`` or backticks occur outside shell quotes."""
    quote: str | None = None
    escaped = False
    for character in command:
        if escaped:
            escaped = False
            continue
        if character == "\\":
            escaped = True
            continue
        if quote is not None:
            if character == quote:
                quote = None
            continue
        if character in {"'", '"'}:
            quote = character
        elif character in {"$", "`"}:
            return True
    return False


def parse_argv(command: str | list[str]) -> list[str]:
    from_list = isinstance(command, list)
    if isinstance(command, list):
        argv = command[:]
    elif isinstance(command, str):
        if len(command) > MAX_COMMAND:
            raise AdapterError("command_too_large")
        if _has_unquoted_shell_operator(command):
            raise AdapterError("shell_operator")
        if _has_unquoted_dynamic_expansion(command):
            raise AdapterError("dynamic_expansion")
        try:
            argv = shlex.split(command, posix=True)
        except ValueError as exc:
            raise AdapterError("command_parse_error") from exc
    else:
        raise AdapterError("command_missing")
    if not argv or len(argv) > 128:
        raise AdapterError("command_empty_or_too_large")
    if any(not isinstance(item, str) or "\0" in item for item in argv):
        raise AdapterError("command_argument_invalid")
    if from_list and (
        any(token in SHELL_OPERATORS for token in argv)
        or any(any(operator in token for operator in (";", "&", "|", ">", "<", "\n", "\r")) for token in argv)
    ):
        raise AdapterError("shell_operator")
    if from_list and any("$" in token or "`" in token for token in argv):
        raise AdapterError("dynamic_expansion")
    return argv


def _command_name(argv: list[str]) -> str:
    return _basename(argv[0]) if argv else ""


def _command_digest(command: Any) -> str:
    """Hash malformed argv values without raising another exception."""
    if isinstance(command, str):
        return _sha256(command)
    if isinstance(command, list):
        return _sha256("\0".join(item if isinstance(item, str) else repr(item) for item in command))
    return _sha256(repr(command))


def _fixed_offset_seconds(timezone: str, clock: dict[str, Any]) -> int | None:
    optional = clock.get("offset_seconds")
    if (
        isinstance(optional, (int, float))
        and not isinstance(optional, bool)
        and math.isfinite(float(optional))
        and int(optional) == optional
        and -86_400 <= int(optional) <= 86_400
    ):
        return int(optional)
    if timezone in {"UTC", "GMT", "Etc/UTC", "Etc/GMT", "Z"}:
        return 0
    raw = timezone
    if raw.startswith("UTC") or raw.startswith("GMT"):
        raw = raw[3:]
    if raw and raw[0] in "+-":
        sign = 1 if raw[0] == "+" else -1
        digits = raw[1:]
        try:
            if ":" in digits:
                hours, minutes = digits.split(":", 1)
                hours_value, minutes_value = int(hours), int(minutes)
                if hours_value > 24 or minutes_value > 59:
                    return None
                return sign * (hours_value * 3600 + minutes_value * 60)
            if len(digits) > 2:
                hours_value, minutes_value = int(digits[:-2]), int(digits[-2:])
                if hours_value > 24 or minutes_value > 59:
                    return None
                return sign * (hours_value * 3600 + minutes_value * 60)
            hours_value = int(digits)
            if hours_value > 24:
                return None
            return sign * hours_value * 3600
        except ValueError:
            return None
    return None


def _effective_anchor_seconds(clock: dict[str, Any]) -> float | None:
    """Resolve the profile clock without silently consulting host time.

    A profile normally describes a frozen observation (`rate == 1`).  A
    moving virtual clock is deterministic only when it carries the real-time
    instant at which the anchor was sampled.  The optional
    ``real_anchor_epoch_seconds`` extension supplies that instant; absent it,
    non-unit rates fail closed rather than drifting from an undocumented
    carrier clock.
    """
    if clock.get("follow_realtime"):
        try:
            now = time.time()
        except (OverflowError, OSError, ValueError):
            return None
        return now if math.isfinite(now) else None
    anchor = float(clock["anchor"])
    rate = float(clock["rate"])
    real_anchor = clock.get("real_anchor")
    if rate not in (0.0, 1.0):
        if real_anchor is None:
            return None
        try:
            real_anchor_value = float(real_anchor)
        except (TypeError, ValueError, OverflowError):
            return None
        if not math.isfinite(real_anchor_value):
            return None
        try:
            elapsed = time.time() - real_anchor_value
            result = anchor + elapsed * rate
        except (OverflowError, OSError, ValueError):
            return None
        return result if math.isfinite(result) else None
    return anchor


def _timezone_for_profile(timezone: str, clock: dict[str, Any]) -> _datetime.tzinfo | None:
    """Build a deterministic fixed-offset tzinfo for the profile.

    Named IANA zones require a pinned tzdata compiler/runtime.  This adapter
    has no way to verify that the host's zoneinfo database matches the profile,
    so it refuses them unless the profile supplies the offset at the anchor.
    The resulting tzname is the profile's name, which keeps `%Z` from exposing
    the carrier's local abbreviation.
    """
    offset = _fixed_offset_seconds(timezone, clock)
    if offset is None and clock.get("follow_realtime"):
        try:
            from zoneinfo import ZoneInfo

            return ZoneInfo(timezone)
        except Exception:
            return None
    if offset is None:
        return None
    if offset == 0 and timezone in {"UTC", "GMT", "Etc/UTC", "Etc/GMT", "Z"}:
        return _datetime.timezone.utc
    try:
        return _datetime.timezone(_datetime.timedelta(seconds=offset), name=timezone)
    except (TypeError, ValueError):
        return None


_DATE_DIRECTIVES = frozenset("aAbBcCdDeFgGhHIjklmMnprRSTtUVwWxXyYzZs%")
_LOCALE_DATE_DIRECTIVES = frozenset("aAbBcApprxX")


def _format_profile_date(instant: _datetime.datetime, fmt: str, locale_name: str) -> str | None:
    """Format a date using only known directives and the profile locale.

    Python's `strftime` otherwise inherits the hook process locale, which can
    make a supposedly virtual date reveal the carrier's language.  We set the
    requested LC_TIME for the duration of this one-process hook and restore it
    immediately.  If the requested locale is unavailable, returning `None` is
    safer than emitting host-localized text.
    """
    # Validate directives before passing user input to strftime.  GNU-only
    # extensions such as `%:z` are intentionally unsupported until a pinned
    # formatter is available.
    index = 0
    locale_sensitive = False
    while index < len(fmt):
        if fmt[index] != "%":
            index += 1
            continue
        if index + 1 >= len(fmt):
            return None
        directive = fmt[index + 1]
        if directive not in _DATE_DIRECTIVES:
            return None
        locale_sensitive = locale_sensitive or directive in _LOCALE_DATE_DIRECTIVES
        index += 2

    # `%e` is not implemented consistently by Python builds.  A sentinel
    # avoids relying on the host libc while retaining POSIX's blank-padded day.
    # Do not use NUL: macOS strftime is a C string and truncates at `\0`.
    sentinel_day = "<<AGS_DAY>>"
    sentinel_zone = "<<AGS_ZONE>>"
    sentinel_offset = "<<AGS_OFFSET>>"
    sentinel_percent = "<<AGS_PERCENT>>"
    format_for_strftime = fmt.replace("%%", sentinel_percent)
    format_for_strftime = format_for_strftime.replace("%e", sentinel_day)
    format_for_strftime = format_for_strftime.replace("%Z", sentinel_zone)
    format_for_strftime = format_for_strftime.replace("%z", sentinel_offset)

    previous_locale: str | None = None
    if locale_sensitive:
        try:
            previous_locale = _py_locale.setlocale(_py_locale.LC_TIME)
            _py_locale.setlocale(_py_locale.LC_TIME, locale_name)
        except (_py_locale.Error, ValueError):
            if previous_locale is not None:
                try:
                    _py_locale.setlocale(_py_locale.LC_TIME, previous_locale)
                except _py_locale.Error:
                    pass
            return None
    try:
        rendered = instant.strftime(format_for_strftime)
    except (OverflowError, ValueError):
        return None
    finally:
        if previous_locale is not None:
            try:
                _py_locale.setlocale(_py_locale.LC_TIME, previous_locale)
            except _py_locale.Error:
                # A failed restore is process-local; the response is still
                # deterministic, and the next hook runs in a fresh process.
                pass
    offset = instant.utcoffset() or _datetime.timedelta(0)
    total_seconds = int(offset.total_seconds())
    sign = "+" if total_seconds >= 0 else "-"
    total_seconds = abs(total_seconds)
    offset_text = f"{sign}{total_seconds // 3600:02d}{(total_seconds % 3600) // 60:02d}"
    zone_name = instant.tzname() or "UTC"
    return (
        rendered
        .replace(sentinel_day, f"{instant.day:2d}")
        .replace(sentinel_zone, zone_name)
        .replace(sentinel_offset, offset_text)
        .replace(sentinel_percent, "%")
    )


def _date_format(profile: dict[str, Any], argv: list[str]) -> str:
    for arg in argv[1:]:
        if arg.startswith("+"):
            return arg[1:]
    return "%a %b %e %H:%M:%S %Z %Y"


def _virtual_output(profile: dict[str, Any], argv: list[str]) -> tuple[str, int, str | None]:
    """Return (stdout, exit_code, failure_reason)."""
    name = _command_name(argv)
    identity = _identity(profile)
    paths = _paths(profile)
    locale = _locale(profile)
    clock = _clock(profile)
    target = _section(profile, "target")
    target_os = _target_os(profile)
    if name == "hostname":
        if len(argv) != 1:
            return "", 2, "hostname options are not pinned by the profile"
        return identity["hostname"] + "\n", 0, None
    if name == "whoami":
        if len(argv) != 1:
            return "", 2, "whoami options are not pinned by the profile"
        return identity["username"] + "\n", 0, None
    if name == "id":
        flags = "".join(arg.lstrip("-") for arg in argv[1:] if arg.startswith("-"))
        if any(flag not in "ugnG" for flag in flags) or any(not arg.startswith("-") for arg in argv[1:]):
            return "", 2, "id arguments are not pinned by the profile"
        if "u" in flags and "g" not in flags and "G" not in flags:
            return f"{identity['uid']}\n", 0, None
        if "g" in flags and "u" not in flags and "G" not in flags:
            return f"{identity['gid']}\n", 0, None
        groups = identity["groups"] or [identity["username"]]
        group_text = ",".join(f"{identity['gid']}({group})" for group in groups)
        return (
            f"uid={identity['uid']}({identity['username']}) "
            f"gid={identity['gid']}({identity['username']}) groups={group_text}\n",
            0,
            None,
        )
    if name == "pwd":
        if any(arg not in {"-L", "-P"} for arg in argv[1:]):
            return "", 2, "pwd arguments are not pinned by the profile"
        return paths["cwd"] + "\n", 0, None
    if name in {"env", "printenv"}:
        env = virtual_env(profile)
        args = argv[1:]
        if name == "env" and args and any(arg.startswith("-") for arg in args):
            return "", 2, "env options are not pinned by the profile"
        if args:
            if any("=" in arg or arg not in env for arg in args):
                return "", 2, "env variable selection is not pinned by the profile"
            return "".join(env[arg] + "\n" for arg in args), 0, None
        return "".join(f"{key}={value}\n" for key, value in env.items()), 0, None
    if name == "locale":
        if len(argv) != 1:
            return "", 2, "locale options are not pinned by the profile"
        return f"LANG={locale['lang']}\nLC_ALL={locale['lc_all']}\nLC_TIME={locale['lc_time']}\n", 0, None
    if name == "uname":
        release = str(target.get("version", "ags-virtual"))
        machine = str(target.get("architecture", "x86_64"))
        os_name = "Linux" if target_os == "linux" else "Darwin"
        selected: list[str] = []
        for arg in argv[1:]:
            if arg == "-a":
                selected = [os_name, identity["hostname"], release, "", machine]
                break
            if not arg.startswith("-"):
                return "", 2, "uname argument is not pinned by the profile"
            for flag in arg[1:]:
                field = {"s": os_name, "n": identity["hostname"], "r": release, "m": machine}.get(flag)
                if field is None:
                    return "", 2, "uname flag is not pinned by the profile"
                selected.append(field)
        if not selected:
            selected = [os_name]
        return " ".join(selected) + "\n", 0, None
    if name == "date":
        if any(arg not in {"-u", "--utc"} and not arg.startswith("+") for arg in argv[1:]):
            return "", 2, "date option is not pinned by the profile"
        anchor = _effective_anchor_seconds(clock)
        if anchor is None:
            return "", 2, "profile clock rate requires real_anchor_epoch_seconds"
        if any(arg in {"-u", "--utc"} for arg in argv[1:]):
            tzinfo: _datetime.tzinfo = _datetime.timezone.utc
        else:
            tzinfo = _timezone_for_profile(clock["timezone"], clock)
            if tzinfo is None:
                return "", 2, "profile timezone requires pinned tzdata or offset_seconds"
        try:
            instant = _datetime.datetime.fromtimestamp(anchor, _datetime.timezone.utc).astimezone(tzinfo)
            rendered = _format_profile_date(instant, _date_format(profile, argv), locale["lc_time"])
            if rendered is None:
                return "", 2, "profile locale or date format is not pinned"
            return rendered + "\n", 0, None
        except (OverflowError, OSError, ValueError):
            return "", 2, "profile clock anchor is outside the supported range"
    return "", 2, "not_a_virtual_command"


def _profile_allowed_host_visible(profile: dict[str, Any], command_name: str) -> bool:
    policy = _section(profile, "policy")
    configured = policy.get("allowed_host_visible", policy.get("allow_host_visible", profile.get("allow_host_visible", [])))
    if isinstance(configured, dict):
        configured = list(configured)
    # Accept the documented list form and the convenient scalar shorthand.
    # A wildcard is still explicit: it applies only to this command gate and
    # never bypasses shell/carrier checks performed before this function.
    if isinstance(configured, str):
        configured = [configured]
    if not isinstance(configured, list):
        return False
    allowed = {str(item) for item in configured}
    return "*" in allowed or command_name in allowed


def _profile_has_host_wildcard(profile: dict[str, Any]) -> bool:
    policy = _section(profile, "policy")
    configured = policy.get("allowed_host_visible", policy.get("allow_host_visible", profile.get("allow_host_visible", [])))
    if isinstance(configured, str):
        return configured == "*"
    if isinstance(configured, list):
        return "*" in {str(item) for item in configured}
    if isinstance(configured, dict):
        return "*" in {str(item) for item in configured}
    return False


def classify(profile: dict[str, Any], command: str | list[str]) -> dict[str, Any]:
    try:
        argv = parse_argv(command)
    except AdapterError as exc:
        return {
            "status": "unknown",
            "decision": "deny",
            "reason": str(exc),
            "command_sha256": _command_digest(command),
        }
    if _contains_hidden_path(argv, _hidden_markers(profile)):
        return {
            "status": "unknown",
            "decision": "deny",
            "reason": "carrier_marker",
            "command_name": _command_name(argv),
            "command_sha256": _sha256("\0".join(argv)),
        }
    if _is_carrier_probe(argv) or (
        _profile_has_host_wildcard(profile)
        and _command_name(argv).lower() in CARRIER_PROBE_INTERPRETERS
    ):
        return {
            "status": "unknown",
            "decision": "deny",
            "reason": "carrier_probe",
            "command_name": _command_name(argv),
            "command_sha256": _sha256("\0".join(argv)),
        }
    name = _command_name(argv)
    if name in SHELL_NAMES:
        return {"status": "unknown", "decision": "deny", "reason": "nested_shell_or_script", "command_sha256": _sha256("\0".join(argv))}
    if name in VIRTUAL_COMMANDS:
        try:
            stdout, exit_code, failure = _virtual_output(profile, argv)
        except (AdapterError, TypeError, ValueError, OverflowError):
            return {
                "status": "unknown",
                "decision": "deny",
                "reason": "profile_invalid",
                "command_name": name,
                "command_sha256": _sha256("\0".join(argv)),
            }
        if failure:
            return {"status": "unknown", "decision": "deny", "reason": failure, "command_name": name, "command_sha256": _sha256("\0".join(argv))}
        if len(stdout.encode("utf-8", "surrogatepass")) > MAX_OUTPUT:
            return {"status": "unknown", "decision": "deny", "reason": "virtual_output_too_large", "command_name": name, "command_sha256": _sha256("\0".join(argv))}
        return {
            "status": "virtual",
            "decision": "allow",
            "reason": "profile_observation",
            "command_name": name,
            "command_sha256": _sha256("\0".join(argv)),
            "stdout": stdout,
            "exit_code": exit_code,
            "replacement": "printf '%s' " + shlex.quote(stdout),
        }
    if _profile_allowed_host_visible(profile, name):
        return {
            "status": "host_visible",
            "decision": "allow",
            "reason": "explicit_profile_allowlist",
            "command_name": name,
            "command_sha256": _sha256("\0".join(argv)),
            "argv": argv,
        }
    return {
        "status": "unknown",
        "decision": "deny",
        "reason": "command_not_profile_backed",
        "command_name": name,
        "command_sha256": _sha256("\0".join(argv)),
    }


def _extract_tool_name(request: dict[str, Any]) -> str:
    for field in TOOL_FIELDS:
        value = request.get(field)
        if isinstance(value, str) and value:
            return value
    return ""


def _extract_command(request: dict[str, Any]) -> str | list[str] | None:
    tool_input = request.get("tool_input", request.get("toolInput", {}))
    if isinstance(tool_input, dict):
        for field in COMMAND_FIELDS:
            value = tool_input.get(field)
            if isinstance(value, (str, list)):
                return value
    for field in COMMAND_FIELDS:
        value = request.get(field)
        if isinstance(value, (str, list)):
            return value
    return None


def _with_command_replacement(tool_input: dict[str, Any], replacement: str) -> dict[str, Any]:
    """Copy tool input and replace the command field that was actually used."""
    updated = dict(tool_input)
    key = next(
        (field for field in COMMAND_FIELDS if field in updated),
        "command",
    )
    updated[key] = replacement
    return updated


def _claude_decision(event: str, decision: str, reason: str, updated: dict[str, Any] | None = None) -> dict[str, Any]:
    """Build Claude's documented PreToolUse decision envelope."""
    output: dict[str, Any] = {
        "hookEventName": event,
        "permissionDecision": decision,
        "permissionDecisionReason": str(reason)[:512],
    }
    if updated is not None:
        output["updatedInput"] = updated
    return {"hookSpecificOutput": output}


def _codex_pre_decision(decision: str, reason: str, updated: dict[str, Any] | None = None) -> dict[str, Any]:
    """Build Codex's PreToolUse envelope independently of Claude's adapter.

    The fields currently overlap with Claude's event, but Codex's parser has
    stricter semantics: an ``allow`` decision must carry ``updatedInput``.
    Keeping this formatter separate makes that requirement visible and avoids
    accidentally inheriting a future Claude-only field.
    """
    output: dict[str, Any] = {
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
        "permissionDecisionReason": str(reason)[:512],
    }
    if updated is not None:
        output["updatedInput"] = updated
    return {"hookSpecificOutput": output}


def _hidden_markers(profile: dict[str, Any]) -> tuple[str, ...]:
    paths = _section(profile, "paths")
    configured = paths.get("hidden_host_markers", profile.get("hidden_host_markers", []))
    values = [str(item).lower() for item in configured] if isinstance(configured, list) else []
    return tuple(dict.fromkeys(values + [item.lower() for item in DEFAULT_CARRIER_MARKERS]))


def _contains_hidden_path(value: Any, markers: tuple[str, ...]) -> bool:
    if isinstance(value, str):
        lowered = value.lower()
        return any(marker and marker in lowered for marker in markers)
    if isinstance(value, dict):
        return any(_contains_hidden_path(item, markers) for item in value.values())
    if isinstance(value, list):
        return any(_contains_hidden_path(item, markers) for item in value)
    return False


def _is_carrier_probe(argv: list[str]) -> bool:
    """Return whether argv asks the host for an untranslatable fact.

    This check runs before the host-visible allowlist.  A wildcard is useful
    for ordinary build tools, but it must not make `/proc`, DMI, hypervisor,
    cloud-metadata, or Windows-interop probes observable by accident.
    """
    if not argv:
        return False
    name = _command_name(argv).lower()
    if name in CARRIER_PROBE_COMMANDS:
        return True
    return any(
        marker in token.lower()
        for token in argv[1:]
        for marker in CARRIER_PROBE_PATH_MARKERS
    )


def _claude_pre(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    tool = _extract_tool_name(request)
    tool_input = request.get("tool_input", request.get("toolInput", {}))
    if not isinstance(tool_input, dict):
        return _deny("tool_input must be an object", "PreToolUse")
    shell_tool = tool.lower() in SHELL_TOOLS
    command = _extract_command(request) if shell_tool else None
    if command is not None:
        result = classify(profile, command)
        if result["status"] == "virtual":
            updated = _with_command_replacement(tool_input, result["replacement"])
            if updated.get("run_in_background"):
                return _deny("virtual observations cannot run in background", "PreToolUse")
            return _claude_decision("PreToolUse", "allow", "AGS profile observation", updated)
        if result["status"] == "host_visible":
            return _claude_decision("PreToolUse", "allow", "explicit profile host-visible allowlist")
        return _deny(result.get("reason", "unknown command"), "PreToolUse")
    if shell_tool:
        return _deny("shell tool input has no command", "PreToolUse")
    if _contains_hidden_path(tool_input, _hidden_markers(profile)):
        return _deny("path identifies the carrier and is not in the target profile", "PreToolUse")
    return {}


def _claude_permission(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    command = _extract_command(request)
    if command is None:
        return _deny("Bash permission request has no command", "PermissionRequest")
    result = classify(profile, command)
    if result["status"] == "virtual":
        tool_input = request.get("tool_input", {})
        updated = _with_command_replacement(tool_input, result["replacement"]) if isinstance(tool_input, dict) else {"command": result["replacement"]}
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow", "updatedInput": updated},
            }
        }
    if result["status"] == "host_visible":
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"},
            }
        }
    return _deny(result.get("reason", "unknown command"), "PermissionRequest")


def _tool_output_replacement(response: Any, stdout: str, exit_code: int) -> Any:
    """Preserve provider-specific output shape while replacing content."""
    if isinstance(response, dict):
        replacement = dict(response)
        if "stdout" in replacement:
            replacement["stdout"] = stdout
        elif "llmContent" in replacement and isinstance(replacement["llmContent"], list):
            replacement["llmContent"] = [{"text": stdout}]
        elif "llmContent" in replacement:
            replacement["llmContent"] = stdout
        elif "content" in replacement:
            replacement["content"] = stdout
        else:
            replacement["stdout"] = stdout
        if "stderr" in replacement:
            replacement["stderr"] = ""
        if "returncode" in replacement:
            replacement["returncode"] = exit_code
        if "exit_code" in replacement:
            replacement["exit_code"] = exit_code
        if "interrupted" in replacement:
            replacement["interrupted"] = False
        return replacement
    return stdout


def _profile_replacement_output(profile: dict[str, Any], command: str | list[str]) -> tuple[str, int] | None:
    """Recognize the exact safe ``printf`` generated by a pre-tool rewrite.

    Claude/Gemini normally pass the *updated* command to their post hook.  A
    stateless hook therefore cannot refer to the original `uname` argv.  We
    accept a replacement only when its shape is exactly the one emitted by
    :func:`classify` and its payload equals a profile observation; arbitrary
    user-authored `printf` commands remain unknown and are never masked as
    virtual.
    """
    if not isinstance(command, str):
        return None
    try:
        argv = parse_argv(command)
    except AdapterError:
        return None
    if len(argv) != 3 or argv[0] != "printf" or argv[1] != "%s":
        return None
    payload = argv[2]
    for candidate in ("hostname", "whoami", "id", "uname", "pwd", "date", "env", "locale", "printenv"):
        try:
            stdout, exit_code, failure = _virtual_output(profile, [candidate])
        except (AdapterError, ValueError):
            continue
        if failure is None and exit_code == 0 and payload == stdout:
            return payload, exit_code
    # `uname -a` and formatted date commands carry arguments in the generated
    # payload, so compare against the common safe forms as well.
    for candidate_argv in (("uname", "-a"), ("date", "-u"), ("date", "+%Y-%m-%d")):
        stdout, exit_code, failure = _virtual_output(profile, list(candidate_argv))
        if failure is None and exit_code == 0 and payload == stdout:
            return payload, exit_code
    return None


def _claude_post(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    command = _extract_command(request)
    if command is None:
        return {}
    result = classify(profile, command)
    if result["status"] != "virtual":
        replacement = _profile_replacement_output(profile, command)
        if replacement is not None:
            result = {
                "status": "virtual",
                "stdout": replacement[0],
                "exit_code": replacement[1],
            }
    if result["status"] != "virtual":
        # The operation already happened.  This is deliberately a block with
        # an explicit post-execution warning, not a claim that the side effect
        # was undone.
        return _deny("post-tool command was not profile-backed; side effect may already have occurred", "PostToolUse")
    response = request.get("tool_response", request.get("toolResponse", ""))
    return {
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": _tool_output_replacement(response, result["stdout"], int(result["exit_code"])),
            "additionalContext": "AGS supplied this observation from the environment profile; the pre-tool replacement is the enforcement point.",
        }
    }


def _gemini_before(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    tool = _extract_tool_name(request)
    shell_tool = tool.lower() in SHELL_TOOLS
    command = _extract_command(request) if shell_tool else None
    if command is not None:
        result = classify(profile, command)
        if result["status"] == "virtual":
            original = request.get("tool_input", request.get("toolInput", {}))
            updated = dict(original) if isinstance(original, dict) else {}
            key = "command" if "command" in updated or "cmd" not in updated else "cmd"
            updated[key] = result["replacement"]
            return {
                "decision": "allow",
                "hookSpecificOutput": {"hookEventName": "BeforeTool", "tool_input": updated},
            }
        if result["status"] == "host_visible":
            return {"decision": "allow", "hookSpecificOutput": {"hookEventName": "BeforeTool"}}
        return _deny(result.get("reason", "unknown command"), "BeforeTool")
    if shell_tool:
        return _deny("shell tool input has no command", "BeforeTool")
    tool_input = request.get("tool_input", request.get("toolInput", {}))
    if _contains_hidden_path(tool_input, _hidden_markers(profile)):
        return _deny("path identifies the carrier and is not in the target profile", "BeforeTool")
    return {}


def _gemini_after(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    command = _extract_command(request)
    if command is None:
        return {}
    result = classify(profile, command)
    if result["status"] != "virtual":
        replacement = _profile_replacement_output(profile, command)
        if replacement is not None:
            result = {"status": "virtual", "stdout": replacement[0], "exit_code": replacement[1]}
    if result["status"] == "virtual":
        # Gemini's documented AfterTool replacement path is decision=deny with
        # the replacement in reason.  It is best-effort because the command has
        # already run; BeforeTool remains mandatory for containment.
        return {
            "decision": "deny",
            "reason": result["stdout"],
            "hookSpecificOutput": {"hookEventName": "AfterTool"},
        }
    return _deny("post-tool command was not profile-backed; side effect may already have occurred", "AfterTool")


def _codex_pre(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    """Format Codex's native PreToolUse wire schema.

    The current Codex hook schema intentionally mirrors Claude for this event:
    `hookSpecificOutput.permissionDecision` and `updatedInput`.  Keeping this
    formatter separate prevents a future provider change from silently making
    one provider accept another provider's envelope.
    """
    tool = _extract_tool_name(request)
    tool_input = request.get("tool_input", request.get("toolInput", {}))
    if not isinstance(tool_input, dict):
        return _deny("tool_input must be an object", "PreToolUse")
    shell_tool = tool.lower() in SHELL_TOOLS
    command = _extract_command(request) if shell_tool else None
    if command is not None:
        result = classify(profile, command)
        if result["status"] == "virtual":
            updated = _with_command_replacement(tool_input, result["replacement"])
            if updated.get("run_in_background"):
                return _deny("virtual observations cannot run in background", "PreToolUse")
            return _codex_pre_decision("allow", "AGS profile observation", updated)
        if result["status"] == "host_visible":
            # Codex rejects permissionDecision:allow without updatedInput.
            # Re-submit the exact input to express an explicit allow without
            # changing the command or exposing another provider's fields.
            return _codex_pre_decision(
                "allow",
                "explicit profile host-visible allowlist",
                dict(tool_input),
            )
        return _deny(result.get("reason", "unknown command"), "PreToolUse")
    if shell_tool:
        return _deny("shell tool input has no command", "PreToolUse")
    if _contains_hidden_path(tool_input, _hidden_markers(profile)):
        return _deny("path identifies the carrier and is not in the target profile", "PreToolUse")
    return {}


def _codex_permission(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    """Format Codex's PermissionRequest schema without unsupported rewrites.

    Codex's native schema reserves `decision.updatedInput` and currently
    rejects it.  A virtual command must therefore have been rewritten by the
    preceding PreToolUse event.  Returning an allow here would otherwise let an
    unrewritten host command through, so the adapter denies and explains the
    missing pre-hook instead.
    """
    command = _extract_command(request)
    if command is None:
        return _deny("Bash permission request has no command", "PermissionRequest")
    result = classify(profile, command)
    if result["status"] == "host_visible":
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"},
            }
        }
    # The permission check runs after PreToolUse has rebuilt the invocation, so
    # it normally sees the exact `printf '%s' <profile-output>` command rather
    # than the original virtual probe.  It is safe to allow that constrained
    # form, but never to use PermissionRequest for the rewrite itself.
    if _profile_replacement_output(profile, command) is not None:
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"},
            }
        }
    if result["status"] == "virtual":
        return _deny("virtual command must be rewritten by Codex PreToolUse", "PermissionRequest")
    return _deny(result.get("reason", "unknown command"), "PermissionRequest")


def _codex_post(profile: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    """Format Codex's distinct PostToolUse schema.

    The wire schema contains an `updatedMCPToolOutput` slot for forward
    compatibility, but the current Codex output parser rejects that field.
    Built-in command results can only be blocked with the top-level
    `decision: "block"`; we never emit either `updatedMCPToolOutput` or
    Claude's `updatedToolOutput`.
    """
    command = _extract_command(request)
    if command is None:
        return {}
    result = classify(profile, command)
    # A Codex PreToolUse hook may hand the rewritten `printf` command to the
    # post hook.  That exact, profile-matching payload is already safe; leave
    # the built-in command result untouched because Codex has no generic output
    # replacement field.  An arbitrary `printf` remains unknown and is blocked
    # below.
    replacement = None
    if result["status"] != "virtual":
        replacement = _profile_replacement_output(profile, command)
        if replacement is not None:
            result = {"status": "virtual", "stdout": replacement[0], "exit_code": replacement[1], "rewritten": True}
    if result["status"] != "virtual":
        return _deny("post-tool command was not profile-backed; side effect may already have occurred", "PostToolUse")
    if replacement is not None:
        return {}
    return {
        "decision": "block",
        "reason": "Codex PostToolUse cannot replace built-in command output; enforce the profile in PreToolUse",
    }


def _context_text(profile: dict[str, Any]) -> str:
    """Build the target view context without carrier or profile-path data."""
    target = _section(profile, "target")
    identity = _identity(profile)
    paths = _paths(profile)
    locale = _locale(profile)
    clock = _clock(profile)
    os_name = "Linux" if _target_os(profile) == "linux" else "macOS"
    distribution = target.get("distribution")
    version = target.get("version")
    architecture = target.get("architecture")
    label = " ".join(str(item) for item in (distribution, version) if isinstance(item, str) and item)
    if label:
        label = f" ({label})"
    arch = f" {architecture}" if isinstance(architecture, str) and architecture else ""
    return (
        f"Target execution environment: {os_name}{label}{arch}. "
        f"User {identity['username']} (uid {identity['uid']}, gid {identity['gid']}) "
        f"on host {identity['hostname']}; working directory {paths['cwd']}; "
        f"home {paths['home']}; timezone {clock['timezone']}; "
        f"locale {locale['lc_all']}."
    )


def context_output(profile: dict[str, Any], provider: str, event: str) -> dict[str, Any]:
    """Return a provider lifecycle hook response containing target context."""
    text = _context_text(profile)
    if provider == "claude":
        return {"hookSpecificOutput": {"hookEventName": event or "SessionStart", "additionalContext": text}}
    if provider == "gemini":
        return {"hookSpecificOutput": {"hookEventName": event or "SessionStart", "additionalContext": text}}
    return {"context": text, "provider": provider, "schema_version": SCHEMA_VERSION}


def _deny(reason: str, event: str) -> dict[str, Any]:
    clean = str(reason).replace("\0", "")[:512]
    if event in {"PreToolUse", "PermissionRequest", "PostToolUse"}:
        if event == "PermissionRequest":
            return {"hookSpecificOutput": {"hookEventName": event, "decision": {"behavior": "deny", "message": clean}}}
        if event == "PostToolUse":
            return {"decision": "block", "reason": clean}
        return {"hookSpecificOutput": {"hookEventName": event, "permissionDecision": "deny", "permissionDecisionReason": clean}}
    return {"decision": "deny", "reason": clean, "hookSpecificOutput": {"hookEventName": event}}


def handle_hook(profile: dict[str, Any], provider: str, request: dict[str, Any]) -> dict[str, Any]:
    event = request.get("hook_event_name", request.get("hookEventName", request.get("event", "")))
    if not isinstance(event, str) or not event:
        return _deny("hook event is missing", "BeforeTool")
    if provider == "claude":
        if event == "PreToolUse":
            return _claude_pre(profile, request)
        if event == "PermissionRequest":
            return _claude_permission(profile, request)
        if event == "PostToolUse":
            return _claude_post(profile, request)
        if event == "SessionStart":
            return context_output(profile, "claude", event)
        return {}
    if provider == "gemini":
        if event == "BeforeTool":
            return _gemini_before(profile, request)
        if event == "AfterTool":
            return _gemini_after(profile, request)
        return {}
    if provider == "codex":
        if event == "PreToolUse":
            return _codex_pre(profile, request)
        if event == "PermissionRequest":
            return _codex_permission(profile, request)
        if event == "PostToolUse":
            return _codex_post(profile, request)
        return {}
    # Generic JSON requests are useful for wrappers and tests.
    command = request.get("argv", request.get("command"))
    if command is None:
        return _deny("generic request has no argv/command", event)
    result = classify(profile, command)
    return {
        "schema_version": SCHEMA_VERSION,
        "provider": provider,
        "status": result.get("status"),
        "decision": result.get("decision"),
        "reason": result.get("reason"),
        "command_name": result.get("command_name"),
        "command_sha256": result.get("command_sha256"),
        **({"replacement": result["replacement"], "stdout": result["stdout"], "exit_code": result["exit_code"]} if result.get("status") == "virtual" else {}),
    }


def run_shell(profile: dict[str, Any], shell_args: list[str]) -> int:
    """Act as the profile's ``$SHELL`` for ``SHELL -c command`` calls."""
    command: str | None = None
    index = 0
    while index < len(shell_args):
        arg = shell_args[index]
        if arg in {"-c", "-lc", "-cl", "--command"}:
            if index + 1 >= len(shell_args):
                print("AGS environment shell: missing command", file=sys.stderr)
                return 2
            command = shell_args[index + 1]
            break
        if arg.startswith("-c") and len(arg) > 2:
            command = arg[2:]
            break
        index += 1
    if command is None:
        print("AGS environment shell only supports -c/-lc", file=sys.stderr)
        return 2
    result = classify(profile, command)
    if result.get("status") == "virtual":
        sys.stdout.write(result["stdout"])
        return int(result["exit_code"])
    print(f"AGS environment shell blocked command ({result.get('reason', 'unknown')})", file=sys.stderr)
    return 126


def run_exec(profile: dict[str, Any], provider: str, argv: list[str], profile_ref: str | None) -> int:
    if not argv:
        raise AdapterError("provider argv is empty")
    if profile_ref in (None, "-"):
        # A provider launch needs a reopenable profile so the child can receive
        # descriptor 198.  Reading a profile from stdin is valid for a one-shot
        # hook, but cannot safely transfer the same bytes to an exec child.
        raise AdapterError("exec_profile_path_required")
    executable = _basename(argv[0]).lower()
    expected = {"codex": {"codex", "codex-cli"}, "claude": {"claude", "claude-code"}, "gemini": {"gemini", "gemini-cli"}}
    if provider in expected and executable not in expected[provider]:
        raise AdapterError("provider executable does not match --provider")
    env = virtual_env(profile, os.environ.items())
    # Do not expose AGS control variables to the provider.  An explicitly
    # invoked hook or shell shim can receive the profile through an inherited
    # descriptor (see `_inherited_profile_fd`), so the model cannot learn the
    # profile path from `env`/`printenv`.
    for key in tuple(env):
        if key.startswith("AGS_"):
            env.pop(key, None)
    # Keep the value profile-native.  Pointing this variable at the adapter's
    # absolute script path would disclose the AGS checkout (and often the
    # carrier's real path) to an otherwise ordinary `echo "$SHELL"` probe.
    # Provider hooks remain the enforcement mechanism; callers that explicitly
    # need the profile shell can invoke `agent-env-shell.py` in `shell` mode.
    cwd = _paths(profile)["cwd"]
    if not os.path.isdir(cwd):
        raise AdapterError("profile_cwd_missing")

    # Resolve the provider before the scrubbed PATH is installed.  POSIX
    # `executable=` lets us execute that resolved binary while presenting a
    # target-native argv[0]; retaining an absolute host install path here would
    # give a trivial `sys.argv[0]` carrier fingerprint.
    resolved = argv[0]
    if os.path.sep not in resolved:
        resolved = shutil.which(resolved) or ""
    if not resolved or not os.path.isfile(resolved) or not os.access(resolved, os.X_OK):
        raise AdapterError("provider_executable_unavailable")

    profile_fd: int | None = None
    installed_profile_fd = False
    try:
        if profile_ref and profile_ref != "-" and os.name == "posix":
            profile_fd = os.open(profile_ref, os.O_RDONLY)
            os.dup2(profile_fd, PROFILE_FD, inheritable=True)
            installed_profile_fd = True
        kwargs: dict[str, Any] = {"cwd": cwd, "env": env, "check": False, "executable": resolved}
        if os.name == "posix" and profile_fd is not None:
            kwargs["pass_fds"] = (PROFILE_FD,)
        child_argv = list(argv)
        if os.path.sep in child_argv[0]:
            child_argv[0] = _basename(child_argv[0])
        completed = subprocess.run(child_argv, **kwargs)
        return int(completed.returncode)
    finally:
        if profile_fd is not None:
            os.close(profile_fd)
        if installed_profile_fd:
            try:
                os.close(PROFILE_FD)
            except OSError:
                pass


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("hook", "command", "context", "shell", "exec"))
    parser.add_argument("--provider", choices=("claude", "gemini", "codex", "generic"), default=None)
    parser.add_argument("--profile", default=None)
    parser.add_argument("--request", help="JSON request file; stdin is used by default")
    return parser


def main(argv: list[str] | None = None) -> int:
    args, remainder = _parser().parse_known_args(argv)
    # Provider argv and shell flags intentionally live after ``--``.  Keeping
    # them in a separate remainder prevents a provider's `--profile` or
    # `--provider` option from being consumed by this adapter.
    args.argv = remainder[1:] if remainder and remainder[0] == "--" else remainder
    provider = args.provider or os.environ.get("AGS_AGENT_PROVIDER", "generic")
    try:
        profile = _read_profile(args.profile)
        if args.mode == "hook":
            raw = sys.stdin.read(MAX_INPUT + 1)
            if len(raw.encode("utf-8", "surrogatepass")) > MAX_INPUT:
                raise AdapterError("request_too_large")
            request = json.loads(raw)
            if not isinstance(request, dict):
                raise AdapterError("request_must_be_object")
            output = handle_hook(profile, provider, request)
            print(_json_line(output))
            return 0
        if args.mode == "command":
            command: str | list[str] = args.argv[1:] if args.argv and args.argv[0] == "--" else args.argv
            result = classify(profile, command)
            print(_json_line({"schema_version": SCHEMA_VERSION, "provider": provider, **result}))
            return 0 if result.get("decision") == "allow" else 2
        if args.mode == "context":
            event = args.argv[0] if args.argv and args.argv[0] != "--" else "SessionStart"
            print(_json_line(context_output(profile, provider, event)))
            return 0
        if args.mode == "shell":
            return run_shell(profile, args.argv)
        command = args.argv[1:] if args.argv and args.argv[0] == "--" else args.argv
        # The profile path is needed only to install the inherited descriptor
        # for `exec`.  It must be resolved before AGS_* variables are removed;
        # hooks and the shell shim then read descriptor 198 instead of an
        # agent-visible control variable.  `--profile` wins over the legacy
        # environment fallback used by direct invocations.
        profile_ref = args.profile or os.environ.get("AGS_ENV_PROFILE")
        if profile_ref and profile_ref != "-":
            profile_ref = str(Path(profile_ref).expanduser().resolve())
        return run_exec(profile, provider, command, profile_ref)
    except (AdapterError, json.JSONDecodeError, TypeError, ValueError, OverflowError, OSError) as exc:
        if args.mode == "hook":
            reason = str(exc) if isinstance(exc, AdapterError) else "adapter_input_invalid"
            print(_json_line(_deny(reason, "PreToolUse")))
            return 0
        print(f"AGS environment adapter: {exc}", file=sys.stderr)
        return 2
    except Exception:
        # Provider hooks must always receive one response.  An unexpected
        # profile/request shape is a deny, never a traceback or an implicit
        # allow caused by a crashed hook process.
        if args.mode == "hook":
            print(_json_line(_deny("adapter_internal_error", "PreToolUse")))
            return 0
        print("AGS environment adapter: adapter_internal_error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
