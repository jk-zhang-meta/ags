# AGS provider environment adapters

`agent_env_adapter.py` is the provider boundary for the environment contract.
It accepts a provider hook request on stdin and writes exactly one JSON object
to stdout. Diagnostics belong on stderr. The adapter never evaluates a shell
string and never copies the parent environment into a hook response.

The command is intentionally separate from the Rust launcher. It can be used
while the provider is installed independently, and it gives AGS one protocol
for the three provider families:

```text
provider request -> normalize tool command -> profile gate
                 -> allow + profile-backed rewrite
                 -> deny (unknown/carrier-visible)
                 -> post-tool mask (audit only; side effects already happened)
```

The contract is observational. A profile-backed rewrite makes a command such
as `uname -a` return the selected profile value. It does not turn an arbitrary
host process into a different kernel and it cannot intercept an absolute native
syscall path. The wrapper therefore fails closed for nested shells, shell
operators, command substitution, and unclassified observations.

## Files and protocol

| File | Purpose |
| --- | --- |
| `scripts/agent_env_adapter.py` | Shared parser, profile reader, command gate, and provider response formatter. |
| `scripts/agent-env-hook.py` | JSONL-compatible entry point for Claude Code and Gemini command hooks. |
| `scripts/agent-env-shell.py` | `SHELL` shim for `-c`/`-lc` invocations. |
| `scripts/agent-env-wrapper.py` | Starts a provider with a scrubbed profile environment. |

Select a profile once with `ags env-use PATH|NAME` (bundled names:
`tokyo-macos`, `tokyo-linux`, `chicago-macos`). Later `ags codex` / `ags claude`
launches materialise an overlay home, rewrite `TZ`/`LANG`/`USER`/`HOSTNAME`,
unset `WSL_*`, and install these hooks. `--environment-profile` overrides that
selection for one launch. Real `HOME`, `PATH`, and cwd stay on the carrier so
the agent can start; kernel, `/proc`, and `gethostname(2)` remain host-visible.

The profile is passed by path (`--profile PROFILE.json`) or by
`AGS_ENV_PROFILE`. Do not use `--profile -` for a hook: the hook request also
uses stdin. The wrapper removes every `AGS_*` variable before starting a
provider and passes the profile through inherited descriptor 198; this keeps
the shell shim usable without exposing a control variable to the provider.
Direct hook and shell invocations may still use `AGS_ENV_PROFILE`. The profile
must be an `environment-contract-v1` JSON object with `target`, `identity`,
`clock`, `locale`, and `paths` sections. `clock.timezone` must be UTC/GMT or a
fixed offset unless the profile supplies a pinned runtime that can provide the
named zone's tzdata; the adapter refuses to guess from the carrier's timezone
database.

For a moving virtual clock, add `clock.real_anchor_epoch_seconds` at the same
sampling instant as `anchor_epoch_seconds`; `clock.rate` then scales elapsed
time from that explicit anchor. A non-unit rate without the real anchor is
rejected, because consulting the carrier clock would make the result
non-deterministic. `rate: 0` is a supported frozen clock.

For a direct, provider-neutral check:

```bash
python3 scripts/agent_env_adapter.py command \
  --provider generic --profile profile.json -- uname -a
```

The response includes `status` (`virtual`, `host_visible`, or `unknown`), a
reason, a command digest, and (for a virtual observation) the replacement and
profile output. It deliberately omits the original command text from audit
metadata; provider-specific `updatedInput` fields contain the replacement
because the provider must execute it.

## Claude Code

Claude Code's command hooks receive JSON on stdin. `PreToolUse` runs before the
tool and is the enforcement point. `PermissionRequest` can make the same
decision when a permission dialog is about to appear. `PostToolUse` can replace
the result visible to the model through `hookSpecificOutput.updatedToolOutput`,
but the tool has already executed by then.

Create a settings fragment (user or project scope as appropriate):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider claude --profile /ABS/profile.json",
            "timeout": 5000
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider claude --profile /ABS/profile.json",
            "timeout": 5000
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider claude --profile /ABS/profile.json",
            "timeout": 5000
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-context.py --provider claude --profile /ABS/profile.json",
            "timeout": 2000
          }
        ]
      }
    ]
  }
}
```

The adapter currently handles `PreToolUse`, `PermissionRequest`, and
`PostToolUse`; an unhandled lifecycle event returns `{}`. If a startup context
is wanted, put the profile's target identity in the provider's system prompt or
use a dedicated context hook. Never put backend provenance (`wsl2`, `vps`, host
names, or host paths) into that context.

For a Bash call whose exact argv is `hostname`, `uname`, `date`, `id`, `pwd`,
`env`, `locale`, `whoami`, or `printenv`, the PreToolUse response has this
shape:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "permissionDecisionReason": "AGS profile observation",
    "updatedInput": {
      "command": "printf '%s' 'Linux\\n'"
    }
  }
}
```

The entire original input object is preserved and only `command` is changed.
Background execution is denied for virtual observations because it would make
the result timing and output ambiguous. Unknown commands and carrier probes
return `permissionDecision: "deny"`, including when Claude is configured with
an otherwise permissive permission mode.

`PostToolUse` returns a shape-preserving `updatedToolOutput` for a known virtual
observation. For built-in Bash output this is an object containing `stdout`,
`stderr`, `interrupted`, and `isImage` where those fields are present in the
original response. A post hook must be treated as redaction/audit only; it does
not undo file writes, network requests, or a command that has already run.

## Gemini CLI

Gemini CLI 0.58.0 exposes `BeforeTool` and `AfterTool` command hooks. Its hook
input uses `tool_name`, `tool_input`, and `tool_response`; output is one JSON
object and exit code 0. A structured `decision: "deny"` blocks a BeforeTool
call. The adapter maps a profile-backed command to
`hookSpecificOutput.tool_input`, which is the Gemini equivalent of Claude's
`updatedInput`.

Add a project or user `.gemini/settings.json` entry:

```json
{
  "hooks": {
    "BeforeTool": [
      {
        "matcher": "run_shell_command|shell|execute_command|Bash",
        "sequential": true,
        "hooks": [
          {
            "name": "ags-environment-gate",
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider gemini --profile /ABS/profile.json",
            "timeout": 5000
          }
        ]
      }
    ],
    "AfterTool": [
      {
        "matcher": "run_shell_command|shell|execute_command|Bash",
        "sequential": true,
        "hooks": [
          {
            "name": "ags-environment-audit",
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider gemini --profile /ABS/profile.json",
            "timeout": 5000
          }
        ]
      }
    ]
  }
}
```

`BeforeTool` allows a virtual command and rewrites the `command` (or `cmd`)
member. It denies nested shells, operators, dynamic expansion, and commands
not named in the profile's host-visible allowlist. `AfterTool` uses Gemini's
documented deny/reason path to hide a result, but it is explicitly late. Hook
configuration is merged from project, user, system, and extension layers; an
untrusted project can therefore change its own hook configuration, so AGS
should install the gate at a trusted user or system layer for an enforcement
claim.

## Codex CLI

Codex CLI 0.153.4 includes native Claude-style hooks. The local Codex source
marks the `hooks` feature stable and enabled by default, and discovers
`$CODEX_HOME/hooks.json` (normally `~/.codex/hooks.json`) plus a repository's
`.codex/hooks.json`. If a deployment disables the feature, enable it in
`config.toml`:

```toml
[features]
hooks = true
```

Install the adapter as a command handler for all three tool events. Codex
matcher patterns are regular expressions, event names are case-sensitive, and
the command-hook `timeout` is in seconds (unlike Claude's millisecond field):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider codex --profile /ABS/profile.json",
            "timeout": 5
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider codex --profile /ABS/profile.json",
            "timeout": 5
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /ABS/AGS/ags/scripts/agent-env-hook.py --provider codex --profile /ABS/profile.json",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

The adapter keeps Codex's three output schemas separate:

| Event | Adapter response | What it can enforce |
| --- | --- | --- |
| `PreToolUse` | `hookSpecificOutput.hookEventName`, `permissionDecision`, `permissionDecisionReason`, and `updatedInput` | Rewrite or deny before the command runs. In the current Codex parser, `permissionDecision: "allow"` requires `updatedInput`; for a host-visible allow the adapter sends the original input unchanged. |
| `PermissionRequest` | `hookSpecificOutput.decision.behavior` (`allow` or `deny`) | Resolve the pending permission. Codex currently reserves `decision.updatedInput` and `updatedPermissions`; emitting either is fail-closed. The exact safe `printf` generated by PreToolUse is allowed; an unrewritten virtual command is denied. |
| `PostToolUse` | Top-level `decision: "block"` plus `reason` when a command was not proven profile-backed | Audit/block after execution. Although the wire schema contains a forward-compatibility `updatedMCPToolOutput` slot, the current Codex parser rejects it; AGS never emits it or Claude's `updatedToolOutput`. Built-in command output cannot be replaced at this stage. |

Codex supplies `session_id`, `turn_id`, `cwd`, `model`, `permission_mode`,
`tool_name`, `tool_input`, and event-specific response fields. The adapter
accepts the same snake-case names and ignores unrelated lifecycle events. The
`PreToolUse` hook is mandatory for environment virtualization; a post hook
cannot undo a file write, network request, or other side effect.

Codex applies trust to unmanaged hook definitions. A new or changed hook can
remain unavailable until its exact definition is approved; do not use
`--dangerously-bypass-hook-trust` when claiming AGS enforcement. The normal
sandbox and approval flags still matter, but they do not replace the hook
configuration.

For installations where native hooks are unavailable, an explicit provider
wrapper remains a useful fallback for environment setup and a smoke test:

```bash
python3 scripts/agent-env-wrapper.py \
  --provider codex --profile /ABS/profile.json -- \
  codex exec --sandbox workspace-write --ask-for-approval on-request \
  'inspect the environment'
```

The wrapper starts the provider with:

* a profile-derived allowlisted environment (`HOME`, `PWD`, `PATH`, identity,
  locale, timezone, and shell);
* no `AGS_*` variables; an explicitly invoked hook or shell shim can receive
  the profile on descriptor 198;
* `SHELL` remains the profile's target-native shell path, so printing `$SHELL`
  does not disclose the AGS checkout. The wrapper itself does not intercept a
  provider that chooses another shell;
* no inherited arbitrary variables or host carrier markers.

The wrapper resolves no command through a shell and passes the provider argv as
an array. It does not intercept a provider that invokes `/bin/bash`, an absolute
binary path, a direct syscall, `/proc`/`/sys`, or a child process after the
provider deliberately discards the wrapper's environment. Codex's native shell
runtime can also choose its configured shell instead of honoring the child's
`SHELL` value. Those paths require a real target backend or a lower-level
runtime broker. AGS must report the capability as unknown/blocked rather than
call the wrapper fully equivalent.

Even with `policy.allowed_host_visible: ["*"]`, the adapter blocks commands
that expose untranslatable carrier facts (`/proc`, `/sys`, DMI/hypervisor and
cloud-metadata probes, Windows interop, and interpreter `-c` escape hatches).
Use a precise audited allowlist when a host-visible diagnostic is intentional.

Provider credentials are intentionally not copied by this wrapper. Use the
provider's normal keychain/configuration mechanism or add a future explicit,
audited credential pass-through list; never broaden the inherited environment
just to make a smoke test pass.

## Shell and command policy

The `shell` mode is useful when a provider honors `SHELL`:

```bash
AGS_ENV_PROFILE=/ABS/profile.json \
  scripts/agent-env-shell.py -lc 'uname -a'
```

It prints only profile output for an exact virtual observation and exits 126 for
an unknown command. It does not interpret pipelines, redirects, command
substitution, or a nested shell. `command`, `eval`, `env VAR=value ...`, and
absolute paths are intentionally not treated as profile observations.

This policy is stricter than a normal interactive shell because the adapter is
an environment gate. If ordinary work is needed, run it on the selected real
Linux/WSL/VPS or macOS backend and let the capability report classify any
host-visible result. Do not weaken the gate by matching command substrings.

## Verification

The provider adapter tests are credential-free and do not invoke an API:

```bash
python3 -m unittest tests/provider_env_adapter_test.py
```

The tests feed synthetic Claude/Gemini hook JSON and verify that only profile
values are returned, malformed input fails closed, shell operators are denied,
and post-tool output is marked as a late replacement. Provider discovery and
real end-to-end prompts are separate evidence: an installed CLI alone does not
prove that its internal tool executor used the AGS hook.

## Source references (accessed 2026-09-06)

* [Claude Code hooks reference](https://code.claude.com/docs/en/hooks):
  `PreToolUse.permissionDecision`, `updatedInput`, `PostToolUse.updatedToolOutput`,
  and the documented fact that PostToolUse runs after execution.
* [Gemini CLI bundled hooks reference](https://github.com/google-gemini/gemini-cli):
  Gemini CLI 0.58.0's installed bundle documents `BeforeTool`, `AfterTool`,
  `hookSpecificOutput.tool_input`, JSON stdin/stdout, and exit code 2 blocking.
* [Codex CLI source](https://github.com/openai/codex):
  `hooks/src/schema.rs` defines distinct `PreToolUseCommandOutputWire`,
  `PermissionRequestCommandOutputWire`, and `PostToolUseCommandOutputWire`
  schemas; `config/src/hook_config.rs` defines `hooks.json` and command-hook
  timeout/trust configuration. The checked-out source is
  `codext/codex-rs/hooks/src/schema.rs` and
  `codext/codex-rs/config/src/hook_config.rs` (Codex CLI 0.153.4).
