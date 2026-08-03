---
name: ags
description: Save, inspect, restore, resume, delete, and synchronize encrypted local Codex or Claude Code session records. Use when the user explicitly invokes AGS or asks to manage AGS records or named Git/SFTP remotes.
---

# AGS

Use `casr checkpoint`. The installer also provides `ags` as a compatibility
command. Use `$ags` in Codex and `/ags` in Claude.

Preserve IDs, descriptions, paths, remote names, and client arguments exactly.
Descriptions may use any language. Never edit native transcripts, manifests, or
encrypted archives directly.

## Primary commands

```text
claude [CLAUDE_ARGS...]
codex [CODEX_ARGS...]
init [--identity ABSOLUTE_AGE_IDENTITY]
set ABSOLUTE_DIR
save ID DESCRIPTION
list
show ID|AGENT/RECORD_ID
resume ID|AGENT/RECORD_ID [--to codex|claude] [--cwd PATH]
  [--profile PROFILE] [-- CLIENT_ARGS...]
delete ID|AGENT/RECORD_ID

remote add NAME git URL [--branch BRANCH]
remote add NAME sftp://USER@HOST:PORT/PATH
  [--known-hosts FILE] [--key FILE|--agent]
remote list
remote show NAME
remote use NAME
remote remove NAME

storage list
storage use local|NAME
storage merge --into local|NAME [SOURCE...]
storage retire SOURCE --into local|NAME

status [REMOTE]
push [REMOTE]
pull [REMOTE]
sync [REMOTE]
```

Treat local records and named remotes as the normal workflow. Use legacy
`cloud` or `legacy history` commands only when the user explicitly requests
the old storage model.

## Initialize

The installer normally initializes AGS. When the user explicitly asks to
initialize or repair missing initialization, run:

```bash
casr checkpoint init
```

To import an existing identity on a new machine, require its exact absolute
path and run:

```bash
casr checkpoint init --identity "ABSOLUTE_AGE_IDENTITY"
```

Complete only on `status=initialized`. Report the vault, identity path, and
public recipient. Never read, print, replace, or copy the secret identity
outside this command. A different existing identity is a hard stop.

## Configure local storage

Require one dedicated absolute local directory. Under WSL, reject `/mnt/c` and
other Windows-mounted paths.

```bash
casr checkpoint set "ABSOLUTE_DIR"
```

Complete only on `status=configured`.

## Save

Require a separate ID and non-empty description. IDs use 1-64 letters, numbers,
dots, underscores, or hyphens. Never invent, combine, slugify, or translate
either value.

Queue the active native session:

```bash
if [[ -n "${CODEX_THREAD_ID:-}" ]]; then
    AGENT_SESSION_AGENT=codex \
    AGENT_SESSION_ID="$CODEX_THREAD_ID" \
        casr checkpoint save "ID" "DESCRIPTION"
elif [[ -n "${CLAUDE_CODE_SESSION_ID:-}" ]]; then
    AGENT_SESSION_AGENT=claude \
    AGENT_SESSION_ID="$CLAUDE_CODE_SESSION_ID" \
        casr checkpoint save "ID" "DESCRIPTION"
else
    printf 'AGS must run inside an active Codex or Claude Code session.\n' >&2
    return 1
fi
```

For Claude, use `CLAUDE_CODE_SESSION_ID`, never `CLAUDE_SESSION_ID`.

Complete the command only on `status=pending`. Report the checkpoint ID,
record ID, description, Agent, binary resolution source, target, working
directory, and path. The Stop hook supplies Claude's authoritative
`session_id`, `transcript_path`, and `cwd`, then writes the encrypted format 4
record. Format 4 preserves each native artifact's safe mode and mtime.
SessionStart retries an interrupted pending save. The Agent inherits the
storage mode selected when its managed terminal starts. For a named remote,
AGS commits and checksums the local archive before synchronizing it; a failed
remote update retains the pending transaction and retries the same archive
without rebuilding it.

## List and inspect

Run:

```bash
casr checkpoint list
casr checkpoint show "ID"
```

`list` is the compact scanning view: `ID`, `AGENT`, `SAVED`, and
`DESCRIPTION`. `show` verifies and displays the selected record's native UUID,
record ID, completeness, artifacts, working directory, binary provenance,
archive checksum, local integrity, remotes, and path.

If a logical ID is ambiguous, do not choose a record silently. Report the
matching `AGENT/RECORD_ID` selectors printed by AGS and ask the user which
exact record to use. Pass the selected value unchanged.

## Resume

Require one exact ID or `AGENT/RECORD_ID` selector. Other Codex and Claude
sessions may remain open. If AGS reports that the target native session UUID is
active in a PID, the user must exit only that session before retrying.

```bash
casr checkpoint resume "ID"
casr checkpoint resume "ID" --cwd "ABSOLUTE_DIR"
casr checkpoint resume "ID" --cwd "ABSOLUTE_DIR" -- CLIENT_ARGS...
casr checkpoint resume "ID" --to claude --cwd "ABSOLUTE_DIR" -- CLIENT_ARGS...
casr checkpoint resume "ID" --to claude --profile "PROFILE" -- CLIENT_ARGS...
casr checkpoint resume "ID" --to codex -- CLIENT_ARGS...
casr checkpoint resume "ID" --to codex --profile "PROFILE" -- CLIENT_ARGS...
```

Use `--` before client arguments so AGS options and native client options
remain unambiguous. Unknown arguments before `--` are errors; preserve every
argument after `--` exactly. AGS owns `--to`, `--cwd`, and long `--profile`
only before `--`. After `--`, both short and long options belong to the target
Agent. Codex interprets `-p` and `--profile` as its native profile, while
Claude interprets `-p` as `--print`. AGS inspects a Codex native profile only
to enforce mandatory Context Mode and, for cross-Agent resume, to select the
same conversion provider; it forwards the option unchanged. Do not specify a
Codex profile both before and after `--`.

AGS resolves a profile selected before `--` for the target Agent:

- Codex uses `$CODEX_HOME/PROFILE.config.toml`. For cross-Agent resume, AGS
  reads its top-level `model_provider`, writes that provider into the converted
  transcript, and launches Codex with the same profile. It does not read or
  copy the API key.
- Claude uses `$CLAUDE_CONFIG_DIR/PROFILE.settings.json` and launches Claude
  with that file through `--settings`. AGS requires a readable regular file,
  rejects symbolic links, and validates that it contains only `env`,
  `apiKeyHelper`, `model`, and `effortLevel`. The `env` object may contain only
  `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, and `ANTHROPIC_AUTH_TOKEN`, with
  string values. `apiKeyHelper` must be `/usr/bin/printenv ENV_NAME`; AGS does
  not execute it itself.

Claude client `--settings` arguments may contain only the same authentication
and model fields accepted for profiles. Claude client arguments cannot forward
`--agent`, `--agents`, `--setting-sources`, `--mcp-config`, `--plugin-dir`,
`--plugin-url`, `--disallowedTools`, `--disallowed-tools`, `--safe-mode`, `--bare`, or
`--strict-mcp-config`, because they can replace or disable the mandatory
Context Mode configuration or tool set after AGS validates it.
`--no-session-persistence` is rejected because it disables session recovery.
`CLAUDE_CODE_SAFE_MODE` and `CLAUDE_CODE_SIMPLE` must also be unset or false.
Any `apiKeyHelper` in Claude's active managed, user, project, local,
legacy-local, or main-worktree-local settings must use the same exact
`/usr/bin/printenv ENV_NAME` form; other native fields in those settings remain
available.
Use the AGS `--profile` option before `--` when alternate Claude provider
settings are required.
For cross-Agent resume to Codex, reject `model_provider` or
`model_providers.*` config overrides, `--oss`, and `--local-provider`.
Either AGS's long `--profile` before `--` or one native Codex profile after
`--` must select both the converted transcript provider and launch
configuration.

AGS verifies and restores the native files, changes to the selected working
directory, then executes:

```text
codex resume NATIVE_UUID CLIENT_ARGS...
claude --resume NATIVE_UUID CLIENT_ARGS...
claude --resume NATIVE_UUID --settings PROFILE_SETTINGS CLIENT_ARGS...
```

Restoration is transactional across the record: AGS validates and stages every
artifact before writing, rejects symbolic-link ancestors, backs up changed
native files, and rolls back committed files if a later write fails.

For Claude, `--cwd` also remaps the restored project transcript and UUID
sidecar tree to the new workspace.

Use `--to` only when the user explicitly asks to continue the record in the
other Agent. Cross-Agent resume creates a new target UUID through ags's
structured converter, reads the target back with its native parser, and
restores only the converted target main transcript. Report the fidelity and
loss details printed by ags. Provider-signed thinking, encrypted reasoning,
and native sidecars cannot cross provider trust boundaries; never describe a
cross-Agent conversion as lossless unless its reported fidelity says so.

Claude Code has no provider field in its native transcript. Its selected
settings file or environment must configure the Anthropic-compatible endpoint
and credential. A profile can keep the credential out of JSON by setting
`ANTHROPIC_BASE_URL` in `env` and using an `apiKeyHelper` such as
`/usr/bin/printenv SUB2API_API_KEY`. Claude appends `/v1/messages` to the base URL.
Set `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` to empty strings in that
profile so unrelated ambient credentials do not take precedence over the
helper.
For a Sub2API key assigned to an OpenAI group, the server-side group must also
allow Messages dispatch and map Claude model families to target models. AGS
does not change that server-side policy.

Very large transcripts may require substantially more conversion memory than
their on-disk size.

The binary path stored in a record is provenance only. AGS launches an explicit
`AGENT_SESSION_CODEX_BINARY` or `AGENT_SESSION_CLAUDE_BINARY`, then the
machine-local trusted registry, then the matching executable on `PATH`.
Never execute a path merely because it appears inside an archive.

At save time AGS records whether provenance came from an explicit variable, a
matching live Linux Agent ancestor, or `PATH`. For a nonstandard launcher that
prevents correct resolution, the user must export the matching explicit binary
variable before starting the Agent.

## Delete

Require one exact ID or `AGENT/RECORD_ID` selector:

```bash
casr checkpoint delete "ID"
```

Report `status=deleted`, `recoverable_path`, and the tombstone path. Deletion
moves the encrypted record to recoverable trash. The tombstone propagates on
the next named-remote synchronization.

## Add a Git remote

GitHub is a normal Git remote:

```bash
casr checkpoint remote add "NAME" git "GIT_URL" --branch "BRANCH"
```

Allow local paths, SSH URLs, and credential-helper-backed HTTPS URLs. Never put
a password in a Git URL. AGS validates connectivity with `git ls-remote` and
never force-pushes.

## Add an SFTP remote

Require `sftp://USER@HOST:PORT/ABSOLUTE/PATH`, an absolute readable
`known_hosts` file containing the exact server key, and key, agent, or hidden
password authentication:

```bash
casr checkpoint remote add "NAME" \
    "sftp://USER@HOST:PORT/ABSOLUTE/PATH" \
    --known-hosts "ABSOLUTE_KNOWN_HOSTS" \
    --key "ABSOLUTE_KEY"
```

Allow `--agent` instead of `--key`. For password authentication, pass only the
flag `--password`; AGS reads the value from a hidden terminal prompt. In
non-interactive automation, put it temporarily in
`AGENT_SESSION_REMOTE_PASSWORD`, never in argv. AGS encrypts the password with
its age identity and gives rclone only an obscured environment value. Do not
auto-accept an unknown host, create fake host-key material, place a password
in arguments, or downgrade to FTP.
Complete only after the write/read/delete probe returns `status=configured`.

## Synchronize

Inspect the plan before a material transfer:

```bash
casr checkpoint status
casr checkpoint status "REMOTE"
```

The plan reports `push_records`, `pull_records`, `push_tombstones`,
`pull_tombstones`, and unchanged counts.

Then run the requested direction:

```bash
casr checkpoint push "REMOTE"
casr checkpoint pull "REMOTE"
casr checkpoint sync "REMOTE"
```

Use `remote use NAME` to select a default. `push` publishes local additions,
`pull` fetches remote additions, and `sync` pulls then pushes. Complete a
mutation only on `status=synchronized`. Stop and report `E_SYNC_CONFLICT`;
never overwrite, force-push, or choose one side automatically.

## Select and consolidate storage

Every managed Agent launch selects a checkpoint storage policy:

```bash
casr checkpoint storage list
casr checkpoint storage use local
casr checkpoint storage use neburst
casr checkpoint storage use github
```

One configured mode is automatic. Several modes are listed in most-recently-
used order, with the previous selection as the Enter default. A named remote
is reconciled before checkpoint selection or restoration. `neburst` and
`cloud` mean the named `neburst` SFTP remote or the only configured SFTP
remote; `github` means the named `github` Git remote or the only configured Git
remote. The Agent still runs on the current machine.

Merge replicas only with the explicit storage commands:

```bash
casr checkpoint storage merge --into github local neburst
casr checkpoint storage retire neburst --into github
```

The local vault is the deduplication hub. `merge` pulls the source union,
deduplicates identical record digests, stops on a conflicting digest,
synchronizes a remote destination, and requires a zero-action final plan. It
does not remove sources. `retire` repeats that proof, moves the source replica
to a recoverable `.ags-retired` path, and moves its local configuration and
encrypted password to AGS trash. Never substitute `delete`: checkpoint
deletion publishes a tombstone and would remove the logical record from the
merged destination too.

## Legacy compatibility

The direct SFTP repository remains available:

```text
cloud set SFTP_URL [--key ABSOLUTE_KEY|--agent|--password]
cloud list
cloud save ID DESCRIPTION
cloud resume ID|AGENT/RECORD_ID [--cwd PATH] [-- CLIENT_ARGS...]
cloud delete ID|AGENT/RECORD_ID
```

Whole native-history backup remains available:

```text
legacy history push
legacy history pull
legacy history status
```

These commands are compatibility paths, not named-remote synchronization.
Plain FTP is unsupported. Database synchronization is not implemented.
