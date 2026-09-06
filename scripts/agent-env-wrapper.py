#!/usr/bin/env python3
"""Launch Codex, Claude Code, or Gemini with a profile-only environment.

The wrapper establishes the process boundary and applies the profile-derived
environment.  It cannot add a hook API to a provider: Claude Code, Gemini CLI,
and current Codex CLI releases expose provider-specific command hooks, which
must be configured as documented in ``docs/PROVIDER_ENV_ADAPTERS.md``.  The
wrapper remains useful as the provider launch boundary and as a fallback for
installations where native hooks are unavailable.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from agent_env_adapter import main  # noqa: E402


if __name__ == "__main__":
    raise SystemExit(main(["exec", *sys.argv[1:]]))
