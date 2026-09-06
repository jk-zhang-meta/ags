#!/usr/bin/env python3
"""Inject the target environment context at a provider session boundary."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from agent_env_adapter import main  # noqa: E402


if __name__ == "__main__":
    raise SystemExit(main(["context", *sys.argv[1:]]))
