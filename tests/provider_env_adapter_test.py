#!/usr/bin/env python3
"""Credential-free tests for the provider environment adapters."""

from __future__ import annotations

import json
import os
import copy
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
HOOK = SCRIPTS / "agent-env-hook.py"
WRAPPER = SCRIPTS / "agent-env-wrapper.py"
SHELL = SCRIPTS / "agent-env-shell.py"


PROFILE = {
    "contract": "environment-contract-v1",
    "id": "test-linux",
    "target": {
        "os": "linux",
        "distribution": "Ubuntu",
        "version": "6.8.0-target",
        "architecture": "x86_64",
    },
    "identity": {
        "hostname": "target-host",
        "username": "target-user",
        "uid": 4242,
        "gid": 4343,
        "groups": ["target-group"],
        "shell": "/bin/bash",
    },
    "clock": {
        "timezone": "UTC+08:00",
        "tzdata": "2026a",
        "anchor_epoch_seconds": 1_735_689_600,
        "offset_seconds": 28_800,
    },
    "locale": {"lang": "zh_CN.UTF-8", "lc_all": "zh_CN.UTF-8", "lc_time": "zh_CN.UTF-8"},
    "paths": {
        "cwd": "/workspace/project",
        "home": "/home/target-user",
        "tmp": "/tmp/target",
        "hidden_host_markers": ["/host/private"],
    },
    "policy": {"allowed_host_visible": []},
}


def run(path: Path, *args: str, stdin: dict | None = None, env: dict[str, str] | None = None) -> tuple[int, list[dict]]:
    child_env = os.environ.copy()
    if env:
        child_env.update(env)
    completed = subprocess.run(
        [sys.executable, str(path), *args],
        input=(json.dumps(stdin) if stdin is not None else None),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=child_env,
        check=False,
    )
    lines = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    return completed.returncode, lines


class ProviderEnvAdapterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        # Keep the profile's cwd real so wrapper tests exercise the same
        # containment check used by a provider launch.  The rest of the paths
        # are virtual values and need not exist on the carrier.
        self.profile = copy.deepcopy(PROFILE)
        self.profile["paths"]["cwd"] = self.directory.name
        self.profile_path = Path(self.directory.name) / "profile.json"
        self.profile_path.write_text(json.dumps(self.profile), encoding="utf-8")

    def tearDown(self) -> None:
        self.directory.cleanup()

    def profile_args(self) -> tuple[str, str]:
        return "--profile", str(self.profile_path)

    def test_claude_pretool_virtualizes_exact_bash_command(self) -> None:
        request = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "uname -a", "description": "probe"},
        }
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        self.assertEqual(len(lines), 1)
        output = lines[0]["hookSpecificOutput"]
        self.assertEqual(output["permissionDecision"], "allow")
        self.assertEqual(output["updatedInput"]["description"], "probe")
        self.assertTrue(output["updatedInput"]["command"].startswith("printf '%s'"))
        self.assertIn("Linux", output["updatedInput"]["command"])

    def test_claude_pretool_denies_shell_operators_and_carrier_paths(self) -> None:
        for command in ("uname -a && whoami", "cat /proc/version"):
            request = {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": command}}
            code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
            self.assertEqual(code, 0)
            decision = lines[0]["hookSpecificOutput"]
            self.assertEqual(decision["permissionDecision"], "deny")
            self.assertNotIn("target-host", json.dumps(lines))

    def test_claude_permission_request_uses_nested_decision_shape(self) -> None:
        request = {
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "date -u +%Y"},
        }
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        decision = lines[0]["hookSpecificOutput"]["decision"]
        self.assertEqual(decision["behavior"], "allow")
        self.assertIn("updatedInput", decision)

    def test_claude_posttool_preserves_response_shape_and_is_late(self) -> None:
        request = {
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "hostname"},
            "tool_response": {"stdout": "wrong", "stderr": "", "interrupted": False, "isImage": False},
        }
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        replacement = lines[0]["hookSpecificOutput"]["updatedToolOutput"]
        self.assertEqual(set(replacement), {"stdout", "stderr", "interrupted", "isImage"})
        self.assertEqual(replacement["stdout"], "target-host\n")
        self.assertIn("pre-tool replacement", lines[0]["hookSpecificOutput"]["additionalContext"])

    def test_gemini_beforetool_rewrites_tool_input(self) -> None:
        request = {
            "hook_event_name": "BeforeTool",
            "tool_name": "run_shell_command",
            "tool_input": {"command": "whoami", "description": "identity"},
        }
        code, lines = run(HOOK, "--provider", "gemini", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0]["decision"], "allow")
        updated = lines[0]["hookSpecificOutput"]["tool_input"]
        self.assertEqual(updated["description"], "identity")
        self.assertIn("printf '%s'", updated["command"])
        self.assertIn("target-user", updated["command"])

    def test_gemini_beforetool_denies_unknown_command(self) -> None:
        request = {
            "hook_event_name": "BeforeTool",
            "tool_name": "run_shell_command",
            "tool_input": {"command": "python3 -c 'print(1)'"},
        }
        code, lines = run(HOOK, "--provider", "gemini", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0]["decision"], "deny")
        self.assertIn("not_profile_backed", lines[0]["reason"])

    def test_legacy_generic_codex_request_is_fail_closed(self) -> None:
        request = {"event": "before_tool", "argv": ["bash", "-c", "uname -a"]}
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        # ``before_tool`` is not a Codex event name.  An unknown native event
        # must produce no permissive decision; wrappers should use the exact
        # PreToolUse schema below.
        self.assertEqual(lines[0], {})

    def test_codex_pretool_use_uses_native_rewrite_schema(self) -> None:
        request = {
            "session_id": "s1",
            "turn_id": "t1",
            "cwd": self.directory.name,
            "hook_event_name": "PreToolUse",
            "model": "gpt-5-codex",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "uname -a", "description": "probe"},
            "tool_use_id": "u1",
            "transcript_path": None,
        }
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        specific = lines[0]["hookSpecificOutput"]
        self.assertEqual(specific["hookEventName"], "PreToolUse")
        self.assertEqual(specific["permissionDecision"], "allow")
        self.assertIn("updatedInput", specific)
        self.assertEqual(specific["updatedInput"]["description"], "probe")
        self.assertIn("Linux", specific["updatedInput"]["command"])

    def test_codex_permission_request_does_not_emit_reserved_updated_input(self) -> None:
        request = {
            "session_id": "s1",
            "turn_id": "t1",
            "cwd": self.directory.name,
            "hook_event_name": "PermissionRequest",
            "model": "gpt-5-codex",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "hostname"},
            "transcript_path": None,
        }
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        decision = lines[0]["hookSpecificOutput"]["decision"]
        self.assertEqual(decision["behavior"], "deny")
        self.assertNotIn("updatedInput", decision)
        self.assertIn("PreToolUse", decision["message"])

    def test_codex_permission_request_allows_exact_pretool_replacement(self) -> None:
        request = {
            "session_id": "s1",
            "turn_id": "t1",
            "cwd": self.directory.name,
            "hook_event_name": "PermissionRequest",
            "model": "gpt-5-codex",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "printf '%s' 'target-host\n'"},
            "transcript_path": None,
        }
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        decision = lines[0]["hookSpecificOutput"]["decision"]
        self.assertEqual(decision["behavior"], "allow")
        self.assertNotIn("updatedInput", decision)

    def test_codex_posttool_use_never_emits_unsupported_output_rewrites(self) -> None:
        base = {
            "session_id": "s1",
            "turn_id": "t1",
            "cwd": self.directory.name,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5-codex",
            "permission_mode": "default",
            "tool_input": {"command": "hostname"},
            "tool_response": {"stdout": "wrong", "stderr": ""},
            "tool_use_id": "u1",
            "transcript_path": None,
        }
        builtin = dict(base, tool_name="Bash")
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=builtin)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0]["decision"], "block")
        self.assertNotIn("hookSpecificOutput", lines[0])
        self.assertNotIn("updatedToolOutput", json.dumps(lines[0]))

        # The current Codex parser rejects its schema's forward-compatibility
        # `updatedMCPToolOutput` member, so AGS must not emit it even for MCP.
        mcp = dict(base, tool_name="mcp__demo__run")
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=mcp)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0]["decision"], "block")
        self.assertNotIn("updatedMCPToolOutput", json.dumps(lines[0]))
        self.assertNotIn("updatedToolOutput", json.dumps(lines[0]))

        # When Codex forwards the PreToolUse-updated command to PostToolUse,
        # the exact generated printf is accepted and the original built-in
        # result is allowed through (there is no generic replacement field).
        rewritten = dict(base, tool_name="Bash", tool_input={"command": "printf '%s' 'target-host\n'"})
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=rewritten)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0], {})

    def test_wildcard_host_visible_allowlist_is_explicit(self) -> None:
        self.profile["policy"]["allowed_host_visible"] = ["*"]
        self.profile_path.write_text(json.dumps(self.profile), encoding="utf-8")
        request = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo profile-approved"},
        }
        code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        specific = lines[0]["hookSpecificOutput"]
        self.assertEqual(specific["permissionDecision"], "allow")
        self.assertEqual(specific["updatedInput"], {"command": "echo profile-approved"})

    def test_wildcard_does_not_allow_carrier_marker_or_unquoted_newline(self) -> None:
        self.profile["policy"]["allowed_host_visible"] = ["*"]
        self.profile_path.write_text(json.dumps(self.profile), encoding="utf-8")
        for command in ("cat /mnt/c/Windows/System32", "echo safe\nuname -a"):
            request = {
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": command},
            }
            code, lines = run(HOOK, "--provider", "codex", *self.profile_args(), stdin=request)
            self.assertEqual(code, 0)
            self.assertEqual(lines[0]["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_shell_shim_only_answers_profile_observation(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(SHELL), *self.profile_args(), "-lc", "uname -a"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertIn("Linux target-host 6.8.0-target", completed.stdout)
        blocked = subprocess.run(
            [sys.executable, str(SHELL), *self.profile_args(), "-lc", "echo $(uname -a)"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(blocked.returncode, 126)

    def test_wrapper_preserves_provider_argv_and_scrubs_environment(self) -> None:
        # A harmless local executable proves that the wrapper passes argv and
        # profile variables without requiring provider credentials or network.
        helper = Path(self.directory.name) / "codex"
        helper.write_text(
            "#!/usr/bin/env python3\n"
            "import json,os,sys\n"
            "print(sys.argv[1:])\n"
            "print(os.environ.get('HOSTNAME'))\n"
            "print(sorted(k for k in os.environ if k.startswith('AGS_')))\n"
            "print(os.environ.get('AGS_ENV_PROFILE'))\n"
            "print(os.environ.get('AGS_SECRET'))\n"
            "try:\n"
            "    with open(198, encoding='utf-8') as f:\n"
            "        print(json.load(f)['identity']['hostname'])\n"
            "except Exception:\n"
            "    print('FD_MISSING')\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        child_env = os.environ.copy()
        child_env["AGS_SECRET"] = "must-not-leak"
        completed = subprocess.run(
            [sys.executable, str(WRAPPER), "--provider", "codex", *self.profile_args(), "--", str(helper), "--flag", "value"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=child_env,
            check=False,
        )
        self.assertEqual(completed.returncode, 0)
        self.assertIn("['--flag', 'value']", completed.stdout)
        self.assertIn("target-host", completed.stdout)
        self.assertIn("[]", completed.stdout)
        # Both AGS control variables are absent; the profile remains usable
        # through the inherited descriptor instead of an environment leak.
        self.assertGreaterEqual(completed.stdout.count("None"), 2)
        self.assertIn("target-host\n", completed.stdout)

    def test_wrapper_env_profile_fallback_still_transfers_descriptor(self) -> None:
        helper = Path(self.directory.name) / "codex"
        helper.write_text(
            "#!/usr/bin/env python3\n"
            "import os\n"
            "print(os.environ.get('AGS_ENV_PROFILE'))\n"
            "try:\n"
            "    with open(198, encoding='utf-8') as f:\n"
            "        print(f.read(128))\n"
            "except Exception:\n"
            "    print('FD_MISSING')\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        child_env = os.environ.copy()
        child_env["AGS_ENV_PROFILE"] = str(self.profile_path)
        child_env["AGS_AGENT_PROVIDER"] = "codex"
        completed = subprocess.run(
            [sys.executable, str(WRAPPER), "--", str(helper)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=child_env,
            check=False,
        )
        self.assertEqual(completed.returncode, 0)
        self.assertIn("None", completed.stdout.splitlines()[0])
        self.assertIn("environment-contract-v1", completed.stdout)

    def test_malformed_hook_json_returns_one_structured_deny(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(HOOK), "--provider", "claude", *self.profile_args()],
            input="not-json",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0)
        output = json.loads(completed.stdout)
        self.assertEqual(output["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_malformed_identity_numbers_fail_closed_with_json(self) -> None:
        for field, value in (("uid", "not-a-number"), ("gid", -1), ("uid", True)):
            profile = copy.deepcopy(self.profile)
            profile["identity"][field] = value
            profile_path = Path(self.directory.name) / f"bad-{field}.json"
            profile_path.write_text(json.dumps(profile), encoding="utf-8")
            request = {
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "whoami"},
            }
            code, lines = run(HOOK, "--provider", "claude", "--profile", str(profile_path), stdin=request)
            self.assertEqual(code, 0)
            self.assertEqual(lines[0]["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_date_profile_timezone_is_used_for_zone_and_offset(self) -> None:
        # 2025-01-01T00:00:00Z, rendered at the profile's UTC+08 offset.
        request = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "date '+%Z %z %Y-%m-%d %H:%M'"},
        }
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        replacement = lines[0]["hookSpecificOutput"]["updatedInput"]["command"]
        self.assertIn("UTC+08:00 +0800 2025-01-01 08:00", replacement)
        self.assertNotIn("UTC +0000", replacement)

    def test_non_unit_clock_rate_requires_explicit_real_anchor(self) -> None:
        profile = copy.deepcopy(self.profile)
        profile["clock"]["rate"] = 2.0
        profile_path = Path(self.directory.name) / "moving-clock.json"
        profile_path.write_text(json.dumps(profile), encoding="utf-8")
        request = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "date -u +%Y"},
        }
        code, lines = run(HOOK, "--provider", "claude", "--profile", str(profile_path), stdin=request)
        self.assertEqual(code, 0)
        self.assertEqual(lines[0]["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_wildcard_blocks_untranslatable_carrier_probes(self) -> None:
        profile = copy.deepcopy(self.profile)
        profile["policy"]["allowed_host_visible"] = ["*"]
        profile_path = Path(self.directory.name) / "wildcard.json"
        profile_path.write_text(json.dumps(profile), encoding="utf-8")
        for command in (
            "cat /proc/cpuinfo",
            "cat /etc/os-release",
            "python3 -c 'print(1)'",
            "sysctl -a",
            "curl http://169.254.169.254/",
        ):
            request = {
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": command},
            }
            code, lines = run(HOOK, "--provider", "codex", "--profile", str(profile_path), stdin=request)
            self.assertEqual(code, 0)
            self.assertEqual(lines[0]["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_wrapper_keeps_target_native_shell_value(self) -> None:
        helper = Path(self.directory.name) / "codex"
        helper.write_text(
            "#!/usr/bin/env python3\n"
            "import os\n"
            "print(os.environ.get('SHELL'))\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        completed = subprocess.run(
            [sys.executable, str(WRAPPER), "--provider", "codex", *self.profile_args(), "--", str(helper)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout.strip(), "/bin/bash")
        self.assertNotIn("agent-env-shell.py", completed.stdout)

    def test_follow_realtime_date_is_current_not_frozen_anchor(self) -> None:
        self.profile["clock"]["follow_realtime"] = True
        self.profile["clock"]["anchor_epoch_seconds"] = 0
        self.profile_path.write_text(json.dumps(self.profile), encoding="utf-8")
        request = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "date -u +%Y"},
        }
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        command = lines[0]["hookSpecificOutput"]["updatedInput"]["command"]
        self.assertNotIn("1970", command)
        self.assertIn("2026", command)

    def test_claude_session_start_injects_target_context(self) -> None:
        request = {"hook_event_name": "SessionStart", "source": "startup"}
        code, lines = run(HOOK, "--provider", "claude", *self.profile_args(), stdin=request)
        self.assertEqual(code, 0)
        context = lines[0]["hookSpecificOutput"]["additionalContext"]
        self.assertIn("target-host", context)
        self.assertIn("target-user", context)
        self.assertNotIn("wsl2", context.lower())


if __name__ == "__main__":
    unittest.main()
