# Native Session Resume Research

Status: research complete; architecture implemented in the current worktree.

Research date: 2026-07-25

Verified versions:

- OpenAI Codex CLI 0.145.0
- Claude Code 2.1.220

## Product Model

AGS should not parse a transcript and imitate an Agent. Its job is:

```text
detect the active native session
  -> capture that Agent's complete session footprint
  -> store an immutable AGS record
  -> restore the footprint into the native state directory
  -> launch the trusted local Agent binary with its native resume command
```

The final commands are:

```bash
codex resume <UUID> [PROMPT] [OPTIONS]
claude --resume <UUID> [OPTIONS] [PROMPT]
```

Anything after the optional AGS `--` separator must remain an argv array. AGS
must consume the separator rather than forwarding it to the Agent.

## Codex

### Required State

Codex can resume a UUID from one valid rollout:

```text
$CODEX_HOME/sessions/YYYY/MM/DD/rollout-...-<UUID>.jsonl
$CODEX_HOME/sessions/YYYY/MM/DD/rollout-...-<UUID>.jsonl.zst
```

Codex reads both plain and compressed rollouts, but current releases resolve
`codex resume <UUID>` through the `threads` table in the newest
`state_*.sqlite`. Therefore:

- the rollout and a matching thread-index row are the minimum native resume
  state;
- `state_5.sqlite`, `-wal`, and `-shm` must not be copied over another
  machine's shared database;
- AGS restores the rollout transactionally, then asks ags to validate the
  final file and upsert only that UUID's row using the live database schema;
  registration failure rolls the restored rollout back.

AGS must preserve the rollout bytes and physical extension. It should not
rebuild a rollout from extracted chat messages.

### Working Directory

Rollouts contain session and turn working directories. AGS should launch from
the saved directory when it still exists. Cross-machine restoration needs an
explicit target directory, such as `ags resume ID --cwd NEW_PATH`; AGS should
not guess a similar path.

Primary evidence:

- [Codex rollout recorder](https://github.com/openai/codex/blob/4c43465133428898aa84f0bfc02c306ed65fb66a/codex-rs/rollout/src/recorder.rs)
- [Codex compression support](https://github.com/openai/codex/blob/4c43465133428898aa84f0bfc02c306ed65fb66a/codex-rs/rollout/src/compression.rs)
- [Codex UUID lookup and database repair](https://github.com/openai/codex/blob/4c43465133428898aa84f0bfc02c306ed65fb66a/codex-rs/rollout/src/list.rs)
- [Codex session-name index](https://github.com/openai/codex/blob/4c43465133428898aa84f0bfc02c306ed65fb66a/codex-rs/rollout/src/session_index.rs)

## Claude Code

### Required State

The minimum state for native resume is the main transcript in the correct
project storage:

```text
$CLAUDE_CONFIG_DIR/projects/<project-key>/<UUID>.jsonl
```

A full-fidelity record also captures UUID-keyed native sidecars when present:

```text
projects/<project-key>/<UUID>/
file-history/<UUID>/
tasks/<UUID>/
session-env/<UUID>/
```

The project session subtree can contain subagent transcripts, tool-result
spillover, and remote-agent metadata. `file-history` preserves undo data,
`tasks` preserves task-v2 state, and `session-env` preserves shell environment
hooks. Capturing only the main JSONL can make `claude --resume` start, but it is
not a full-fidelity restore.

Claude's project layout is internal and may change between releases. The Agent
adapter therefore needs versioned fixtures and a real native resume test for
supported Claude releases.

### Detection and Working Directory

AGS should prefer the lifecycle hook's `session_id`, `transcript_path`, and
`cwd`. The supported environment fallback is `CLAUDE_CODE_SESSION_ID`.

Claude resolves a session within the current project and its Git worktrees.
Restoration must map the bundle to the destination directory's project storage,
then launch from that directory. AGS must not silently rewrite every path-like
string in the transcript.

Primary evidence:

- [Claude Code session management](https://code.claude.com/docs/en/sessions)
- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)
- [Claude Code environment variables](https://code.claude.com/docs/en/env-vars)
- [Claude-Session-Backup full restore scopes](https://github.com/DazzleML/Claude-Session-Backup/blob/7c8f2158d53c45dc21cb4b283e562e4f69d5b064/claude_session_backup/git_ops.py)

## Cross-Agent Conversion

Cross-Agent resume is a separate path implemented by ags's structured
Claude/Codex converter. It creates a fresh target-native UUID and reports both
its claimed and independently verified fidelity. Provider trust boundaries
still impose real limits:

- Codex encrypted reasoning and Claude signed thinking cannot cross provider
  trust boundaries;
- Claude sidechains and native sidecars are omitted;
- native token accounting and provider-specific sidecars do not cross.

AGS never points the converter at a real Agent home. It invokes the current
`casr --json resume` in empty temporary homes with `--no-store`, validates the
single reported target transcript, normalizes the selected target working
directory and Codex `model_provider`, parses the final native file again, then
sends only that main transcript through AGS's existing restore transaction.
Warnings and structured loss records from ags are surfaced to the user.

The selected Codex profile—whether supplied as an AGS option before `--` or a
native Codex option after `--`—is also passed to `codex resume`, so custom
OpenAI-compatible providers such as Sub2API cannot diverge between stored
session metadata, the live Codex thread index, and runtime configuration. AGS
reads no provider secret.

## Binary Provenance

A record should retain:

```text
invoked path
resolved real path
version
SHA-256
OS and architecture
```

This data explains what created the session; it does not authorize execution.
An archive fetched from Git or SFTP must not be able to select an arbitrary
absolute executable path.

Resume should select a binary from machine-local trust:

1. an explicitly configured local Agent binary;
2. the current trusted `PATH` entry;
3. a saved path only when it resolves to the same locally trusted identity.

A version or digest change should produce a warning because normal CLI upgrades
must remain usable. An Agent, platform, or executable identity mismatch must
stop. AGS should never archive and execute an old Agent binary.

## Baseline Implementation Gap

Before the format 4 redesign, the dirty worktree already added useful behavior:

- separate logical ID and description;
- explicit `codex` or `claude` metadata;
- saved executable path;
- native resume invocation and argv-preserving `"$@"`;
- physical record IDs and ambiguity errors;
- `column`-based alignment.

That baseline still had these material gaps:

1. A format 3 checkpoint contains only `manifest` and one `session.jsonl`.
2. Codex discovery misses `.jsonl.zst`.
3. Claude detection uses `CLAUDE_SESSION_ID`, not
   `CLAUDE_CODE_SESSION_ID`, and does not retain the hook transcript path.
4. Claude UUID sidecars are not captured or restored.
5. Resume requires the old absolute working directory and has no mapping.
6. Archive metadata can directly influence the executable path.
7. A standalone AGS `--` is forwarded instead of consumed.
8. The default ten-column list is too wide; there is no `ags show`.
9. Format 2 exact filenames can hide mixed-version records with the same
   logical ID.
10. Fake CLI tests prove file copying and argv construction, not native context
    recovery.

## Storage Findings

The simplest consistent model is local-first:

```text
Agent adapter -> immutable AGS record -> local vault
                                          |
                                      push / pull
                                          |
                                   Git or SFTP remote
```

The storage adapter sees opaque records, not Codex or Claude paths. Sync uses
record IDs and SHA-256, never mtime:

- same record ID and same digest is idempotent;
- same record ID and different digest is a hard conflict;
- same logical ID and different record IDs preserves both;
- a missing object is not deletion;
- deletion propagates through an immutable tombstone.

GitHub should be treated as a Git remote, not a separate session backend.
Git must never force-push. GitHub rejects objects over 100 MiB, and encrypted
session bundles can exceed that size, so large records must use SFTP.

SFTP should reuse the existing rclone transport, require host-key validation,
upload through partial names, publish the manifest last, and verify the
downloaded digest. Plain FTP is excluded.

A generic database payload backend is not justified without a concrete
database and operational need. SQLite may later be a rebuildable search index.
A PostgreSQL payload adapter should wait for measured object sizes, a managed
database, TLS and credential policy, backup ownership, and a reason that
Git/SFTP is insufficient.

Primary evidence:

- [GitHub repository limits](https://docs.github.com/repositories/creating-and-managing-repositories/repository-limits)
- [GitHub large-file limits](https://docs.github.com/repositories/working-with-files/managing-large-files/about-large-files-on-github)
- [Rclone SFTP backend](https://rclone.org/sftp/)
- [Git credential storage](https://git-scm.com/docs/gitcredentials)
- [SQLite limits](https://www.sqlite.org/limits.html)
- [PostgreSQL binary data](https://www.postgresql.org/docs/current/datatype-binary.html)

## Accepted Architecture

The accepted implementation direction is:

- the official checksummed RMUX 0.9.1 release, or an already complete compatible
  stable 0.9.x installation at or above 0.9.1, as AGS's mandatory
  live-terminal runtime for interactive Claude Code and Codex commands;
- a local-first vault with optional named Git and SFTP remotes;
- one Agent seam with concrete Codex and Claude adapters;
- one Storage seam operating on opaque immutable records;
- a structured, checksummed multi-artifact record format that preserves safe
  file modes and mtimes;
- preflighted, staged restore with symlink-ancestor rejection and rollback;
- session-scoped restore exclusion: Codex open-rollout descriptors and Claude
  PID/start-time registries block only the exact native UUID being resumed;
- format 2/3 read compatibility, marked as minimum-fidelity;
- compact `ags list` plus detailed `ags show`;
- live-session `ags sessions`, `ags attach`, and `ags describe` commands;
- cross-Agent CLI: `casr checkpoint resume SELECTOR --to codex|claude [--cwd PATH] [--profile PROFILE] [-- AGENT_ARGS...]`;
- no dynamic plugin framework, plain FTP, or database payload adapter yet.

The user accepted this direction. Format 4, local-first named Git/SFTP
synchronization, compact list/show output, and the Codex/Claude adapters are
implemented in the current worktree. Storage selection, most-recently-used
ordering, pre-launch reconciliation, post-save remote synchronization, and
verified merge/retirement are also implemented. The RMUX vertical slice is
implemented: `ags claude`, `ags codex`, checkpoint resume, and ags
`--launch` all cross one structured RMUX boundary; bare `ags` attaches the
only private AGS session or presents an activity-ordered keyboard picker. Plain FTP,
a generic database payload adapter, and a dynamic plugin framework remain
deliberately deferred.

The parameter boundary is explicit. After `ags claude` or `ags codex`, every
argument—including tokens named `--to`, `--cwd`, or `--profile`—belongs to
that native Agent. For checkpoint resume, AGS parses only
the known options before an independent `--`; every argument after it belongs
to the restored Agent. Unknown pre-separator arguments fail instead of
silently changing which side owns the remaining command line. AGS still
validates Agent arguments without renaming or reordering them.

Claude `--settings FILE` and `--settings=JSON` remain native Claude arguments,
but AGS validates their contents before launch. They may contain only provider
authentication (`ANTHROPIC_BASE_URL`, API key/token, or `apiKeyHelper`) plus
`model` and `effortLevel`. Settings that can change hooks, MCP servers,
plugins, permissions, or tool availability fail closed so the AGS plugin
cannot be disabled from under the launch. `apiKeyHelper` is limited to
`/usr/bin/printenv ENV_NAME`; arbitrary helper commands are rejected.
AGS applies that helper restriction to Claude's active managed, user, project,
local, legacy-local, and main-worktree-local settings files too. Other fields
in those native settings sources remain available. The helper check runs at the
managed-launch boundary.

An optional description belongs to AGS only in the prefix form
`ags --description TEXT claude|codex ...`. A token named `--description` after
`claude` or `codex` is forwarded unchanged to that Agent, like every other
Agent argument.

## Terminal Runtime

AGS owns a private RMUX server named `ags`. RMUX 0.9.1's Unix compatibility
path shell-quotes the pane command internally, so AGS never places an Agent
binary or Agent-controlled argument in that command. It gives RMUX only a
trusted AGS trampoline and carries the structured payload as JSON in the
client environment:

```text
rmux -L ags -f /dev/null new-session -A -s <opaque-name>
  -c <cwd> -- <casr-absolute-path> terminal-payload

AGS_RMUX_LAUNCH_PAYLOAD={
  "program": "<agent>",
  "args": ["<arg-1>", "<arg-2>"],
  "env": [["KEY", "VALUE"]]
}

terminal-payload -> execve(<agent>, <args>, <env>)
```

The requesting client's environment carries both the JSON payload and its
structured environment overrides. AGS does not duplicate either into RMUX
arguments visible to process listings.

`new-session -A` is the lifecycle primitive. It creates the PTY and runs the
Agent once when no matching runtime exists; otherwise it attaches to the
existing PTY and ignores the new payload. Session names contain only the fixed
`ags-` prefix and a truncated SHA-256 of AGS's internal runtime key, so working
directories, descriptions, native session IDs, and Agent arguments do not
appear in `list-sessions`. AGS accepts only stable RMUX 0.9.x releases at or
above 0.9.1; older releases, newer incompatible contracts, and unrecognized
version output fail closed. Installation atomically replaces the
client/daemon/helper files but never signals a running daemon, so existing
terminals continue on that daemon until it exits naturally.

Human-readable live metadata is stored separately under
`${XDG_STATE_HOME:-~/.local/state}/ags/live-sessions/<opaque-name>.json` with a
private directory and file mode. RMUX remains the source of truth for liveness,
so stale metadata is never listed or attached. `ags sessions` exposes the
opaque ID, Agent, description, and cwd; `ags attach ID` and
`ags describe ID DESCRIPTION` accept a full ID or an unambiguous prefix.

When more than one terminal is live, bare `ags` uses Up/Down and Enter to
select it. Esc exits. Del requires explicit `y` confirmation and terminates
only the selected live RMUX session; it does not delete a checkpoint. TTY
`ags sessions` uses the same picker, including for a single session. TTY
`ags list` uses the same keys for checkpoint resume and the existing
recoverable checkpoint deletion path. Non-TTY `ags sessions` and `ags list`
remain stable tables for scripts.

The two recovery tiers are deliberately different:

```text
outer terminal closes
  -> RMUX daemon and PTY still alive
  -> bare `ags` attaches; shell/SSH/editor/Agent process state is unchanged

RMUX daemon or machine stops
  -> PTY processes no longer exist
  -> AGS restores the native checkpoint, cwd, and launch argv
  -> Agent resumes from its native session, but process memory is not restored
```

Public commands do not expose `new`; RMUX creation remains an implementation
detail, while `ags attach ID` provides precise recovery. Read-only and
automation commands such as non-TTY `list`, `show`, `status`,
`sync`, hooks, JSON output, and launch dry-runs do not allocate a terminal.
The mandatory-terminal contract covers interactive launches made through
`ags` and ags `--launch`. Raw `claude`/`codex` invocations and the installer's
optional `cc`/`cod` compatibility aliases remain direct Agent commands, so
scripts and non-interactive Agent modes are not silently moved into a PTY.

RMUX is initially integrated through its stable CLI because that code owns raw
TTY mode, resizing, rendering, and detach behavior. The Rust SDK remains a
future option for pane events or checkpoint triggers, not a replacement for the
CLI attach path.

## Storage Selection and Consolidation

“Local”, “Neburst”, and “GitHub” are checkpoint synchronization policies, not
places where the Agent process implicitly runs. The Agent always runs on the
machine where `ags` is executed; running it on Neburst requires first entering
that machine, for example over SSH.

The implemented storage UX follows these rules:

- one configured mode is selected without a prompt;
- several configured modes appear in a short most-recently-used list, with the
  last choice as the Enter default;
- selecting Neburst or GitHub performs safe pull/reconcile before checkpoint
  selection;
- a successful save first commits one encrypted local archive, records its
  SHA-256 in the pending transaction, then automatically synchronizes the
  selected remote; a failed remote update retains that exact pending archive
  for retry;
- a successful delete commits a recoverable local move plus a tombstone and
  immediately synchronizes the selected remote; failure retains one named
  pending sync operation instead of silently reporting remote success. Flush
  synchronization and retry acknowledgement share one storage transaction, so
  a concurrent failure cannot have its replacement retry removed;
- consolidation copies the union of immutable records to a destination,
  removes byte-identical duplicates, and stops on a same-record-ID/different-
  digest conflict;
- GitHub is updated through normal non-force Git commits and pushes; Neburst is
  updated through verified SFTP transfers;
- source replicas are never removed automatically. After the destination has a
  zero-action synchronization plan, AGS may offer the separate
  `ags storage retire SOURCE --into DESTINATION` operation;
- a verified merge immediately redirects old source-mode names to the
  destination, including names exported by already-running RMUX terminals.
  Sources stay recoverable until retirement, can be made active again by a
  reverse merge, and cannot be removed or reused while a redirect exists;
- retirement first repeats merge verification, then moves the source replica
  into a recoverable `.ags-retired` location and moves its local remote
  configuration to AGS trash. It does not create checkpoint tombstones. Any
  interrupted retirement blocks other consolidation until that exact
  transaction is resumed.

The public commands are:

```text
ags storage list
ags storage use local|NAME
ags storage merge --into local|NAME [SOURCE...]
ags storage retire SOURCE --into local|NAME
```

`cloud` and `neburst` resolve the named `neburst` remote or the only configured
SFTP remote. `github` resolves the named `github` remote or the only configured
Git remote. Named SFTP remotes accept key, agent, or hidden password
authentication. Password authentication reads from the terminal or
`AGENT_SESSION_REMOTE_PASSWORD`, encrypts the value with the AGS age identity,
and supplies only rclone's obscured environment form to that transport.
Direct SSH receives plaintext only through an inherited `sshpass` file
descriptor; it is never stored in JSON, argv, or logs. SFTP synchronization
requires a verified host key and an SSH server shell with `flock` and standard
file utilities.

The legacy `.cloud` configuration is automatically registered as the reserved
named `neburst` remote. Legacy root-level `codex/` and `claude/` archives are
included in the same content revision, imported into the unified `ags-v1`
catalog, and moved with that revision during retirement. Legacy
`ags cloud delete` first publishes the unified tombstone and only then moves
the direct-SFTP copy to recoverable cloud trash. It resumes an already
published, content-bound tombstone by exact Agent/record ID, and moves the
active encrypted archive after its optional manifest so every interrupted
partial move remains selectable or retryable.

AGS repeats the exact-session check before and during native-file replacement
and again before launch. Codex and Claude do not expose a cooperative
cross-process restore lock, so a native client can still open the UUID inside
the final check-to-rename syscall window. Detection after a replacement triggers
the existing rollback, but no external detector can make that last window
atomic; callers must not launch the same UUID concurrently.

## Release Oracle

Fixture tests must cover both Codex rollout extensions, Claude main and sidecar
trees, path mapping, binary trust, argv boundaries, destination conflicts,
metadata preservation, partial-write rollback, symlink escape rejection, legacy
records, sync conflicts, and 80/120-column output.

Cross-Agent fixtures must additionally cover both directions, new target UUIDs,
compressed Codex input, isolated converter homes and environment, output
read-back, converted-thinking normalization, ignored converter metadata,
unsupported-version refusal, and zero real-home writes on conversion failure.

Before describing restoration as complete, run one isolated real session per
Agent:

1. create a session containing a random sentinel and tool result;
2. save it through AGS;
3. remove the native footprint from the isolated Agent home;
4. prove native resume fails;
5. restore through AGS and verify every declared artifact hash;
6. let AGS launch the official resume command;
7. ask the Agent to recall the sentinel and prior tool result;
8. verify the original UUID continues rather than creating a new session.

The fixture oracle passes. Separate real transport E2E checks also passed for
local restore, a temporary GitHub branch, and a temporary Neburst SFTP
directory, using fake Agent executables and real storage transports.

On 2026-07-30, the RMUX vertical slice passed four additional layers:

- the verified upstream RMUX 0.9.1 Linux artifact created, detached, and
  reattached an AGS PTY while preserving the pane PID and shell state;
- a second `new-session -A` invocation for the same runtime ignored a payload
  that would have failed if executed, proving attach does not duplicate the
  Agent;
- Codex 0.146.0 and Claude Code 2.1.220 each returned a unique sentinel, were
  detached, and were recovered by bare `ags` with the same pane PID;
- full-fidelity encrypted checkpoints of those real native sessions restored
  the original UUIDs and displayed the prior sentinel conversation inside a
  new managed RMUX terminal;
- repeating the exact checkpoint resume for both Agents attached the same pane
  PID with exactly one live session and did not run restore a second time;
- hostile-looking arguments containing spaces and shell metacharacters reached
  the final Agent process literally, while RMUX received only the trusted AGS
  trampoline command.

The automated checks passed 759 library tests, 86 CLI E2E tests, 44 JSON
contract tests, 10 launch-spec tests, the complete AGS shell self-check, the
offline installer smoke test, and Clippy with warnings denied.

On 2026-07-25, an isolated paid cross-Agent sentinel also passed against Codex
0.145.0 and Claude Code 2.1.220. Each current native-format source fixture
contained a unique token and was saved through AGS. AGS converted it, generated
a different target UUID, and launched the target client's official
non-interactive resume path. Claude recalled `AGS_CODEX_TO_CLAUDE_7F3C91` from
target session `29d29dcc-ef36-4e3d-8861-ccf594f9527f`; Codex recalled
`AGS_CLAUDE_TO_CODEX_2A6D84` from target session
`019f9906-b8f7-7cb2-85d5-386512c066d4`. Both clients ran with tools disabled in
temporary homes, temporary authentication copies were destroyed, and neither
target UUID appeared in the normal Agent homes.

This proves that both target clients accept the converted native transcript and
receive its conversation context. It does not make provider-signed thinking or
foreign tool calls portable, so the implementation remains described as
native-resume compatible rather than "perfect restore". The broader same-Agent
artifact-continuity checklist above remains a separate oracle.
