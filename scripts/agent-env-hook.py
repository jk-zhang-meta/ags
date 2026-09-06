#!/usr/bin/env python3
"""Executable provider hook entry point for AGS."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from agent_env_adapter import main  # noqa: E402


if __name__ == "__main__":
    raise SystemExit(main(["hook", *sys.argv[1:]]))
