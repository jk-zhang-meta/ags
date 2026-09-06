#!/usr/bin/env python3
"""Stdlib tests for the credential-free agent environment harness."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "agent-env-probe.py"


def invoke(*args: str, env: dict[str, str] | None = None) -> tuple[int, list[dict]]:
    child_env = os.environ.copy()
    if env:
        child_env.update(env)
    completed = subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=child_env,
        check=False,
    )
    lines = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    return completed.returncode, lines


class AgentEnvContractTests(unittest.TestCase):
    def test_dry_run_is_jsonl_and_does_not_start_provider(self) -> None:
        code, lines = invoke("--agent", "codex", "--agent", "claude", "--agent", "gemini", "--dry-run")
        self.assertIn(code, (1, 2))  # planned probes are unknown without execution
        self.assertEqual(lines[0]["kind"], "header")
        discoveries = [line for line in lines if line["kind"] == "discovery"]
        self.assertEqual({line["provider"] for line in discoveries}, {"codex", "claude", "gemini"})
        self.assertTrue(all(line["invoked"] is False and line["network"] is False for line in discoveries))

    def test_profile_id_and_command_are_not_echoed(self) -> None:
        secret = "profile-secret-do-not-print"
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False) as handle:
            json.dump({"profile_id": "linux-ubuntu"}, handle)
            profile = handle.name
        try:
            code, lines = invoke("--profile", profile, "--command", "git status", "--dry-run", env={"AGS_SECRET": secret})
        finally:
            Path(profile).unlink()
        rendered = json.dumps(lines)
        self.assertNotIn(secret, rendered)
        self.assertEqual(next(line for line in lines if line["kind"] == "header")["profile_id"], "linux-ubuntu")
        self.assertEqual(code, 2)  # no executed evidence can establish equivalence

    def test_run_never_shells_or_dumps_environment(self) -> None:
        secret = "super-secret-env-value"
        code, lines = invoke("--run", "--command", f"{sys.executable} -c 'print(\"ok\")'", env={"SECRET": secret})
        rendered = json.dumps(lines)
        self.assertNotIn(secret, rendered)
        command = next(line for line in lines if line["kind"] == "command")
        self.assertTrue(command["executed"])
        self.assertIn(command["status"], {"host_visible", "unknown", "native"})
        self.assertNotEqual(code, 0)  # negative leak probes remain non-equivalent on a native host

    def test_malformed_profile_is_error_jsonl(self) -> None:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False) as handle:
            handle.write("not-json")
            profile = handle.name
        try:
            code, lines = invoke("--profile", profile)
        finally:
            Path(profile).unlink()
        self.assertEqual(code, 2)
        self.assertEqual(lines[0]["kind"], "error")

    def test_dynamic_shell_is_unknown_and_not_executed_in_dry_run(self) -> None:
        code, lines = invoke("--command", "echo $(uname -a)", "--dry-run")
        command = next(line for line in lines if line["kind"] == "command")
        self.assertEqual(command["status"], "unknown")
        self.assertEqual(command["reason"], "dynamic_expansion")
        self.assertFalse(command["executed"])
        self.assertEqual(code, 2)

    def test_profile_hash_can_confirm_a_virtual_observation(self) -> None:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False) as handle:
            json.dump({"cwd": "/tmp", "commands": {"pwd": {"status": "virtual", "stdout": "/tmp\n"}}}, handle)
            profile = handle.name
        try:
            code, lines = invoke("--profile", profile, "--run", "--command", "pwd")
        finally:
            Path(profile).unlink()
        command = next(line for line in lines if line["kind"] == "command")
        self.assertEqual(command["status"], "virtual")
        self.assertNotEqual(code, 0)  # host-visible negative probes still block the profile


if __name__ == "__main__":
    unittest.main()
