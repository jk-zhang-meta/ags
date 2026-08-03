# casr

<div align="center">
  <img src="casr_illustration.webp" alt="casr - Cross Agent Session Resumer">
</div>

Cross Agent Session Resumer for coding agents: resume a session created in one provider (Claude Code, Codex, Gemini, and more) using a different provider by converting through a canonical session model.

![Rust](https://img.shields.io/badge/Rust-2024%20nightly-orange)
![Status](https://img.shields.io/badge/status-active-green)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-blue)

## Quick Install (Recommended)

```bash
curl -fsSL "https://raw.githubusercontent.com/jk-zhang-meta/ags/main/install.sh?$(date +%s)" | bash
```

That installer is the primary distribution path. It handles platform detection, secure artifact verification, fallback source builds, shell completions, and agent-oriented local setup in one step.

## TL;DR

**The Problem**: AI coding sessions are siloed by provider. A useful Codex session cannot be resumed directly in Claude Code, and vice versa.

**The Solution**: `casr` discovers a session across installed providers, reads it into a canonical IR, writes a verified native session for supported targets, and prints the exact resume command.

### Why Use casr?

| Feature | What It Does |
|---|---|
| Cross-provider resume | `casr cc resume <codex-session-id>` and similar conversions in one command |
| Canonical IR | Normalizes provider formats into a common model, then exports back to native format |
| Native-format writers | Produces plausible provider-native session files, not intermediate-only exports |
| Safety-first writes | Atomic temp-then-rename writes, conflict detection, optional `.bak` backup with `--force` |
| Provider auto-detection | Finds which provider owns a session ID without user guesswork |
| Verification step | Re-reads written output to catch writer bugs before you try to resume |
| Machine-friendly output | `--json` mode for scripts and automation |
| Encrypted checkpoints | Save, restore, delete, and synchronize encrypted Codex/Claude records with `casr checkpoint` |
| Debuggability | `--verbose`, `--trace`, and structured tracing with `RUST_LOG` |

## Quick Example

```bash
# 1) See what providers are available
casr providers

# 2) Find a session from any provider
casr list --limit 20 --sort date

# 3) Inspect a single session
casr info 019c3eae-94c3-7d73-9b2a-9edb18f1563b

# 4) Convert that session to Claude Code format
casr cc resume 019c3eae-94c3-7d73-9b2a-9edb18f1563b

# ergonomic shorthand (auto-detects source provider from the session ID)
casr -cc 019c3eae-94c3-7d73-9b2a-9edb18f1563b   # open in Claude Code
casr -cod 019c3eae-94c3-7d73-9b2a-9edb18f1563b  # open in Codex
casr -gmi 019c3eae-94c3-7d73-9b2a-9edb18f1563b  # open in Gemini CLI

# 5) Resume in Claude Code using the generated ID
claude --resume <new-session-id>
```

## Encrypted Checkpoints

`ags` includes AGS's encrypted checkpoint, identity, Git/SFTP synchronization,
tombstone, transactional restore runtime, and RMUX-managed live terminals:

```bash
ags                         # attach the most recently active live session
ags claude                  # start Claude Code in an AGS terminal
ags codex                   # start Codex in an AGS terminal
ags claude --model opus     # everything after "claude" belongs to Claude Code
ags codex --model o3        # everything after "codex" belongs to Codex
ags save release-fix "Continue the release fix"
ags list                    # pick a saved session and open it
ags show release-fix
ags resume release-fix
ags resume release-fix --to claude -- --model sonnet
ags delete release-fix
```

For `ags resume`, AGS options such as `--to`, `--cwd`, and `--profile` must
precede an independent `--`; arguments after it are forwarded to the restored
Agent in their original order. Direct launch has no mixed namespace:
even a token named `--to`, `--cwd`, or `--profile` after `ags claude` or
`ags codex` is an Agent token and is never consumed by AGS.

### Agent arguments are remembered

A checkpoint records the Agent arguments its session was started with, so a
session started as `ags codex --dangerously-bypass-approvals-and-sandbox` comes
back with that flag rather than under different permissions than it was working
under.

`ags resume ID` replays them. Passing `--` is how you decide instead: arguments
after it replace the saved ones, and `--` with nothing after it starts the
session with none. `ags show ID` prints what a checkpoint carries.

### Which directory it opens in

A checkpoint also records the directory its session was working in. When that is
a different absolute path from the one you are resuming from, `ags resume` shows
both and asks; when they are the same path it says nothing:

```
  This session was saved in a different directory.

    1) /home/you/work/api          here
    2) /Users/you/work/api         saved with the session

  Open in [1]:
```

This is what makes a synchronized checkpoint resumable on another machine at
all: the recorded path usually does not exist there, and that used to be fatal.
`--cwd PATH` answers the question in advance. Without a terminal to ask on — a
script, a pipe — the recorded directory is used, unchanged.

`ags list` opens a picker — Up/Down or `j`/`k` to move, Enter to open, `a` to
edit the arguments for this launch, Del to delete, Esc to quit. The arguments
for the highlighted session are always shown, because checkpoints synchronize
between machines and a replayed command line is another machine's decision
taking effect here. Replayed arguments pass the same Context Mode checks a typed
one does; a synchronized record is not a way around them.

Every managed launch selects one checkpoint storage mode:

```bash
ags storage list
ags storage use local
ags storage use neburst
ags storage use github
```

Local storage is always the deduplication vault. `neburst` resolves the named
Neburst SFTP remote (or the only configured SFTP remote), while `github`
resolves the named GitHub Git remote (or the only configured Git remote). With
one configured mode AGS selects it automatically. With several, AGS shows a
short list ordered by most recent use and makes the previous choice the Enter
default. A remote choice is reconciled before AGS selects or restores a
checkpoint.

A successful save first commits its encrypted local archive and then
automatically synchronizes the selected Neburst or GitHub remote. If that
network update fails, AGS retains a checksummed pending transaction and retries
the same archive; it does not rebuild or duplicate the checkpoint. Deletion
uses the same rule: the local record is moved to recoverable trash, a tombstone
is committed, and the selected remote is updated immediately or gets one
explicit pending retry. `ags set` refuses to switch the local vault while either
kind of transaction is pending; `ags flush` completes and acknowledges each
retry under the same storage transaction, without dropping a concurrent
replacement retry.

Several storage modes can be consolidated explicitly:

```bash
ags storage merge --into github local neburst
ags storage retire neburst --into github
```

`merge` pulls every source into the local vault, deduplicates identical
record digests, stops on conflicts, synchronizes the destination, and verifies
that no push, pull, or tombstone remains. After verification, future saves from
both new terminals and already-running terminals that still name an old source
are redirected to the destination. The old replica remains recoverable and its
name cannot be removed or reused until `retire`; a reverse merge can make a
preserved source the destination again. `retire` repeats verification, moves
the old remote replica to a recoverable `.ags-retired` location, and moves its
local configuration (including any encrypted SFTP password) to AGS trash.
Interrupted retirement must be resumed before another consolidation. This is
intentionally different from deleting a checkpoint, which publishes a logical
tombstone to every replica.

Detach from a live Agent with RMUX's `Ctrl-b d`. The Agent and its shell remain
alive; running `ags` later attaches to that same PTY. If the RMUX daemon or
machine has stopped, AGS restores a saved checkpoint and starts the Agent's
native resume command in a new managed terminal. AGS does not claim to preserve
process memory across a reboot.

The installer configures Codex and Claude lifecycle hooks and also installs
`ags`. It installs the official checksummed RMUX 0.9.1 client, daemon, and
private helper as one rollback-safe transaction; an already complete compatible
stable 0.9.x installation at or above 0.9.1 is preserved. Interactive AGS
commands require a compatible
[RMUX 0.9.x release](https://rmux.io/docs/get-started/). Installation never
signals or restarts an existing RMUX daemon; replaced binaries take full effect
after that daemon exits naturally. Checkpoint support
requires Linux or macOS, Bash 4+, `age`, `jq`, `rclone`, `rsync`, `zstd`,
OpenSSH, Git, GNU file tools, and `flock`; conversion-only commands remain
available without those tools. Use
`--identity /absolute/path/to/identity.agekey` during installation to import an
existing encryption identity.

Context Mode is a required AGS runtime, not an optional integration. The online
installer and every online `ags init` resolve npm's current stable `latest`
release from the official registry. AGS records its exact version, tarball, and SHA-512
integrity; installs the exact version with npm lifecycle scripts disabled into
a sibling staging directory; validates the reviewed upstream native
provisioner; runs that provisioner in an isolated environment until the
package tree stops changing; and only then activates the versioned runtime under
`${XDG_DATA_HOME:-~/.local/share}/ags/context-mode/runtimes/<platform>-<arch>-node<abi>/<version>`.
A Node ABI change gets its own runtime and requires one online `ags init`;
offline initialization fails closed when its last-good target does not match.
A candidate that
cannot be fetched, validated, configured, or health-checked never replaces the
last-good release. Normal Agent launches use that last-good release without a
network lookup.

npm integrity is supplemented by an AGS SHA-256 manifest over the complete
runtime package tree: every regular file and every safe in-package symbolic link,
including nested dependencies and the active native ABI binding.
Initialization and launch verify that tree and the Agent-managed plugin cache.
This requires Node.js 22.5.0 or newer and `jq`; online refresh also requires
`npm`. `ags init` repairs the registrations and runs Context Mode's
initialization lifecycle for every installed Claude/Codex Agent. Online
initialization runs the official `doctor`; an offline reinstall with a
previously committed last-good runtime performs an `index`/`search` round trip
against Context Mode's persistent SQLite/FTS store without making a network
request.
Context Mode currently has no public `init` subcommand, so AGS defines
initialization as official plugin convergence plus one of those upstream health
paths.

For Claude, AGS also starts a short-lived real Claude process whose model API
endpoint is redirected to loopback before every managed launch and requires the effective SessionStart protection
marker, connected Context MCP server, both core `ctx_*` tools, and exact plugin
version. Claude `--settings` accepts only provider authentication plus model
fields; `apiKeyHelper` is limited to `/usr/bin/printenv ENV_NAME`. Hooks, MCP,
plugin, permission, and tool settings fail closed. The same helper restriction
is enforced for active managed, user, project, and local settings files while
their other native Claude fields remain available. AGS performs this check
before Context Mode initialization and verification can start Claude, and
again at the managed-launch boundary. Other
managed Claude arguments that can replace the selected Agent, settings, MCP,
plugins, or Context tools are rejected, including `--agent`, `--agents`,
`--setting-sources`, `--mcp-config`, `--plugin-dir`, `--plugin-url`,
`--disallowedTools`, `--safe-mode`, `--bare`, and `--strict-mcp-config`.

The integration has no opt-out: `--no-configure` and `--no-skill` only skip
the optional AGS aliases, skills, hooks, and checkpoint bootstrap covered by
those flags. They do not skip Context Mode.
Claude is ready after installation. Codex launches fail closed until its six
Context Mode lifecycle hooks have been reviewed in Codex's official UI. Run
`ags context review-codex`, choose **Review hooks**, trust only
`Plugin - context-mode@context-mode`, exit Codex, and retry the original AGS
command. AGS never uses `--dangerously-bypass-hook-trust` or writes Codex's
private trust hashes.

Context Mode's databases remain in the Agent-native locations
(`${CLAUDE_CONFIG_DIR:-~/.claude}/context-mode` and
`${CODEX_HOME:-~/.codex}/context-mode`). They are not included in AGS
checkpoint synchronization, because they can contain indexed project and
conversation data. AGS checkpoints still restore the native Agent session;
Context Mode then supplies its own continuity data on that machine.

Named SFTP storage requires a verified `known_hosts` entry and an SSH server
shell with `flock` plus standard file utilities. Password mode additionally
requires `sshpass`: rclone receives only its obscured environment value, while
direct SSH receives the plaintext through an inherited file descriptor, never
through argv, logs, or storage JSON. An existing legacy `ags cloud set`
configuration is registered as named `neburst`; its old `codex/` and `claude/`
archives are content-verified and imported into the unified `ags-v1` store.
`ags cloud delete` also goes through the unified tombstone path before moving
the legacy copy to recoverable cloud trash, so a later sync cannot resurrect
the record. Cleanup is exact-record and retryable after interruption: a newer
checkpoint may reuse the old logical ID without being selected, and the active
encrypted archive is moved last so every partial move remains recoverable.

AGS forces RMUX for interactive Agent launches made through `ags` and ags
`--launch`. Direct `claude`/`codex` commands and the optional `cc`/`cod`
compatibility wrappers remain direct executables for scripts and
non-interactive modes; using them explicitly opts out of AGS terminal
management.

## Design Philosophy

1. **Provider fungibility over lock-in**: sessions are portable assets.
2. **Native fidelity over lossy export**: writers target real provider session formats.
3. **Safety over convenience**: atomic writes, conflict checks, read-back verification.
4. **Permissive conversion over brittle strictness**: warnings for imperfect input when conversion is still useful.
5. **Observability by default**: rich logs and actionable errors for every pipeline stage.

## How casr Compares

| Capability | casr | Manual copy/paste | Read-only session search tools | Ad-hoc one-off scripts |
|---|---|---|---|---|
| Convert sessions between providers | Yes | No | No | Partial |
| Provider-native output files | Yes | No | No | Usually brittle |
| Auto-detect source provider by session ID | Yes | No | Sometimes | Rare |
| Atomic writes and conflict handling | Yes | No | N/A | Rare |
| Round-trip testable architecture | Yes | No | N/A | Rare |
| Structured JSON mode for automation | Yes | No | Sometimes | Depends |

## Supported Providers

| Provider | Alias | Read | Write | Resume command |
|---|---|---|---|---|
| Claude Code | `cc` | Yes | Yes | `claude --resume <session-id>` |
| Codex | `cod` | Yes | Yes | `codex resume <session-id>` |
| Antigravity | `agy` | Yes | Yes* | `agy --conversation <conversation-id>` |
| Gemini CLI | `gmi` | Yes | Yes | `gemini --resume <session-id>` |
| Cursor | `cur` | Yes | No | `cursor .` |
| Cline | `cln` | Yes | Yes* | `cline --id <session-id>` |
| Aider | `aid` | Yes | Yes | `aider --chat-history-file <path> --restore-chat-history` |
| Amp | `amp` | Yes | Yes | `amp threads continue <session-id>` |
| OpenCode | `opc` | Yes | Yes* | `opencode --session <session-id>` |
| ChatGPT | `gpt` | Yes | No | `open "https://chatgpt.com/c/<session-id>"` |
| ClawdBot | `cwb` | Yes | Yes | `clawdbot tui --session agent:main:<lowercase-session-id>` |
| Vibe | `vib` | Yes | Yes | `vibe --resume <session-id>` |
| Factory | `fac` | Yes | Yes | `droid --resume <session-id>` |
| OpenClaw | `ocl` | Yes | Yes* | `openclaw tui --session <session-key>` |
| Pi-Agent | `pi` | Yes | Yes | `pi --session <path-to-session.jsonl>` |
| Kiro CLI | `kr` | Yes | Yes | `kiro-cli --resume-id <session-id>` |
| Grok Build | `grk` | Yes | Yes* | `grok --resume <session-id>` |

Providers marked `Write: No` are valid sources and can resume their existing
sessions, but casr refuses to target them—even with `--force` or `--dry-run`—
until a vendor-supported, natively resumable import path is verified.

Notes:
- Initial core focus is Claude Code, Codex, and Gemini CLI.
- Additional providers are implemented through the same `Provider` trait model.
- Antigravity target writes require Python and Google's official
  `google-antigravity>=0.1.9` SDK. casr replays source user/model turns through
  the SDK's local harness and a loopback deterministic model endpoint, then
  reopens the generated SQLite conversation through the SDK and verifies every
  stored step. `agy` resumes that native database directly. The public SDK has
  no direct system/tool/assistant injection API, so adjacent messages are
  visibly labelled and coalesced where necessary; a trailing unanswered
  user-side turn is refused instead of inventing an assistant reply. Set
  `AGS_ANTIGRAVITY_PYTHON` when the SDK is installed in a non-default Python
  environment.
- `OpenCode` target writes require its official `opencode` CLI in `PATH` (or
  `OPENCODE_BIN`). casr delegates import and rollback to the vendor CLI and
  never edits `opencode.db` directly.
- Aider target writes create a dedicated
  `.aider.chat.history.ags-<session-id>.md` and launch Aider with that exact
  file. They never append to the shared `.aider.chat.history.md`.
- Grok target writes require the official `grok` CLI in `PATH` (or `GROK_BIN`).
  casr writes Grok's documented authoritative `summary.json` and
  `updates.jsonl`, then requires the vendor CLI to discover and export the
  session. Failed verification is rolled back through `grok sessions delete`;
  derived history and search indexes remain owned by Grok.
- OpenClaw target writes require the official `openclaw` CLI in `PATH` (or
  `OPENCLAW_BIN`), a running authenticated Gateway granting `operator.admin`,
  and the `sessions.create`,
  `chat.inject`, `chat.history`, `sessions.patch`, and `sessions.delete` RPCs
  introduced in OpenClaw 2026.7.2. ags creates the session and verifies
  it through the Gateway, then uses archive-and-delete for rollback; it never
  edits OpenClaw's SQLite database or session index. This import is deliberately
  marked lossy: OpenClaw stores every imported turn as a gateway-injected
  assistant note with a visible `ags:v1:<role>` label, so the text and labels
  survive but the original user/system/tool role semantics do not.
- Cline target writes require the official `cline` CLI in `PATH` (or
  `CLINE_BIN`). casr uses Cline's local Hub `client.register` →
  `session.create` → official `session.messages` read-back lifecycle and uses
  `session.delete` for rollback; it never edits Cline's database or indexes
  directly. The write is unavailable when the official CLI is not installed.

## Installation

### Primary Path: Hardened `curl | bash` Installer

```bash
curl -fsSL "https://raw.githubusercontent.com/jk-zhang-meta/ags/main/install.sh?$(date +%s)" | bash
```

What this installer does for you:

| Capability | Behavior |
|---|---|
| Platform targeting | Detects Linux/macOS + x86_64/aarch64 and picks the right artifact |
| Supply-chain checks | Verifies SHA256 and Sigstore/cosign when available |
| Download fallback chain | Versioned release -> latest release naming variants -> source build |
| Airgap install | `--offline <tarball>` uses a local AGS artifact and requires a previously committed, revalidated Context Mode last-good runtime |
| Proxy-aware networking | Uses `HTTPS_PROXY` / `HTTP_PROXY` automatically |
| Shell UX | Installs completions for bash/zsh/fish |
| Agent setup | Installs mandatory Context Mode, conversion/checkpoint skills, checkpoint hooks, `ags`, and optional `cc`/`cod`/`gmi` wrappers |

High-value installer flags:

| Flag | Purpose |
|---|---|
| `--verify` | Runs post-install self-test |
| `--force` | Reinstall even if same version is already present |
| `--offline <tarball>` | Reinstall AGS without network access after Context Mode has completed one trusted online initialization |
| `--from-source` | Build from source directly |
| `--system` | Put the binary in `/usr/local/bin`; run as the target non-root user and only when that directory is already writable |
| `--easy-mode` | Auto-update PATH in shell rc files |
| `--yes` | Non-interactive prompt acceptance |
| `--no-configure` | Skip optional agent skill/wrapper setup; Context Mode remains mandatory |
| `--no-skill` | Skip Claude/Codex skill installation |
| `--identity <file>` | Import an existing AGS age identity |

```bash
# Examples
bash install.sh --verify
# Mandatory Context Mode is per-user, so do not run this example with sudo.
bash install.sh --system --easy-mode --yes
bash install.sh --offline ./casr-x86_64-unknown-linux-musl.tar.xz
bash install.sh --no-configure --no-skill
```

A fresh machine must complete one online `ags init` against the official npm
registry before it can use `--offline`. AGS deliberately does not trust a
copied Context Mode runtime cache or its self-carried hashes as a first-use
trust anchor. After a successful activation, later offline reinstalls and
initializations revalidate the committed last-good manifest, full runtime tree,
native binding, and Agent plugin caches before use.

Run `bash install.sh --help` for the full option set.

### Alternative: From Source

```bash
git clone -b ags https://github.com/jk-zhang-meta/ags
cd ags
cargo build --release
./target/release/casr --help
```

### Alternative: Cargo Local Install

```bash
cargo install --path .
casr --help
```

### Alternative: Development Mode

```bash
cargo run -- --help
```

## Quick Start

1. Confirm provider detection.
```bash
casr providers
```

2. List discoverable sessions.
```bash
casr list --sort date --limit 50
```

3. Inspect the source session.
```bash
casr info <session-id>
```

4. Convert to your target provider.
```bash
casr <target-alias> resume <session-id>
```

5. Resume in target provider.
```bash
# Examples
claude --resume <new-session-id>
codex resume <new-session-id>
gemini --resume <new-session-id>
```

## Commands

Global flags:

```bash
--dry-run                 # Show what would happen without writing
--force                   # Overwrite existing target session (creates .bak backup)
--json                    # Structured JSON output
--verbose                 # Debug-level logging (casr=debug)
--trace                   # Trace-level logging (casr=trace)
--source <alias_or_path>  # Explicit source provider alias or direct session path
--enrich                  # Add optional synthetic context/orientation messages
```

### `casr <target> resume <session-id>`

Convert a source session into target provider format and print the target resume command.

```bash
casr cc resume 019c3eae-94c3-7d73-9b2a-9edb18f1563b
casr claude resume 019c3eae-94c3-7d73-9b2a-9edb18f1563b   # standard name fallback
casr cod resume 40f2cb68-fed7-4cee-83de-2b63ba9b7813 --dry-run
casr codex resume 40f2cb68-fed7-4cee-83de-2b63ba9b7813 --dry-run
casr gmi resume 40f2cb68-fed7-4cee-83de-2b63ba9b7813 --source cc
casr gemini resume 40f2cb68-fed7-4cee-83de-2b63ba9b7813 --source claude
casr cc resume <session-id> --force
casr cc resume <session-id> --json
```

**Context budget (opt-in).** By default nothing is trimmed: the whole session
crosses, and anything the conversion cannot carry is reported as a loss and
graded. Pass a cap only when you want one, and whatever it removes is still
counted against the fidelity grade:

```bash
--max-context-tokens <n>  # Cap the transferred history (0 = no cap). Oldest turns go first.
--max-tool-output <n>     # Elide the middle of each tool observation (0 = no cap).
--drop-reasoning          # Drop the source agent's reasoning traces.
```

Each flag removes only what it names. `--max-tool-output` will not delete
reasoning, and `--drop-reasoning` will not drop turns. Reasoning is worth
dropping on a cross-agent handoff — the target cannot replay another agent's
hidden reasoning — but that is a decision, not a side effect of asking for
something else.

`--drop-reasoning` is not an exemption from `--max-context-tokens` in reverse
either: reasoning that belongs to a turn the token cap removes goes with that
turn, and is reported as a reasoning loss when it does.

`--keep-reasoning` is still accepted and now names the default, so an existing
`casr` command line keeps working and keeps getting what it asked for. Passing
it together with `--drop-reasoning` is refused.

`--dry-run` grades the conversion it describes: the same track, the same budget,
the same losses the real run would report. The one thing it cannot report is
`verified_fidelity`, because nothing was written to read back.

### `casr list`

List sessions across installed providers.

```bash
casr list
casr list --provider codex
casr list --workspace /data/projects/myapp
casr list --limit 100 --sort messages

# default behavior (no args): current workspace only, top 10, styled table output
casr list
```

### `casr info <session-id>`

Show non-converting session details.

```bash
casr info 019c3eae-94c3-7d73-9b2a-9edb18f1563b
casr info 019c3eae-94c3-7d73-9b2a-9edb18f1563b --json
```

### `casr providers`

Show provider detection and installation evidence.

```bash
casr providers
```

### `casr completions <shell>`

Generate shell completions.

```bash
casr completions bash > /tmp/casr.bash
casr completions zsh > "${fpath[1]}/_casr"
casr completions fish > ~/.config/fish/completions/casr.fish
```

## Configuration

`casr` is primarily configured by environment variables.

```bash
# Optional provider home overrides for non-standard locations
export CLAUDE_HOME="$HOME/.claude"
export CODEX_HOME="$HOME/.codex"
export GEMINI_HOME="$HOME/.gemini"
export CURSOR_HOME="$HOME/.config/Cursor"
export CLINE_HOME="$HOME/.config/Code/User/globalStorage/saoudrizwan.claude-dev"
export AIDER_HOME="$HOME/.aider"
export OPENCODE_HOME="$HOME/.opencode"
# Amp has no casr override: its data dir moves only with XDG_DATA_HOME.
export XDG_DATA_HOME="$HOME/.local/share"

# Logging verbosity (alternative to --verbose / --trace)
export RUST_LOG="casr=debug"
# or:
export RUST_LOG="casr=trace"
```

### Which variable belongs to whom

Two different kinds of variable appear above and below, and mixing them up is
how you end up reading the wrong directory:

- **casr's own overrides** aim casr at a tree without touching the agent. Most
  of the `*_HOME` names are casr's alone — the agent does not read them.
- **the agent's own variables**, which casr also honours, so that relocating an
  agent the supported way does not hide its sessions from casr.

Where both exist, casr's override wins, so pointing casr somewhere never
disturbs the agent the rest of your shell talks to. An empty value counts as
unset. Note the semantics differ: several of the agents' variables name a *home
directory* that the agent then appends a subdirectory to, so casr appends the
same one.

| Provider | casr's own override | The agent's own variable, also honoured |
|---|---|---|
| Claude Code | `CLAUDE_HOME` | `CLAUDE_CONFIG_DIR` → used as `~/.claude` |
| Codex | — | `CODEX_HOME` → used as `~/.codex` |
| Gemini CLI | `GEMINI_HOME` | `GEMINI_CLI_HOME` → `$GEMINI_CLI_HOME/.gemini` |
| Cline | `CLINE_HOME` | `CLINE_DATA_DIR`, else `$CLINE_DIR/data` |
| Factory (`droid`) | `FACTORY_HOME` | `FACTORY_HOME_OVERRIDE` → `…/.factory/sessions` |
| Pi | `PI_AGENT_HOME` | `PI_CODING_AGENT_DIR` → used as `~/.pi/agent` |
| OpenCode | `OPENCODE_HOME`, `OPENCODE_DB_PATH` | `OPENCODE_DB` (absolute path, or a filename under OpenCode's data dir) |
| ClawdBot | `CLAWDBOT_HOME` | `CLAWDBOT_STATE_DIR` → `$CLAWDBOT_STATE_DIR/sessions` |
| Aider | `AIDER_HOME` | `AIDER_CHAT_HISTORY_FILE` (a file, not a directory) |
| Grok | `GROK_BIN` (CLI path) | `GROK_HOME` → used as `~/.grok` |
| Kiro (CLI store) | — | `KIRO_HOME` → used as `~/.kiro` |
| Kiro (IDE store) | — | none exists — the IDE always uses `~/.kiro`, see below |
| Amp | — | `XDG_DATA_HOME` → `$XDG_DATA_HOME/amp` |
| Vibe | — | `VIBE_HOME` → `$VIBE_HOME/logs/session` |
| OpenClaw | `OPENCLAW_BIN` (CLI path) | `OPENCLAW_STATE_DIR`, else `$OPENCLAW_HOME/.openclaw` |
| Cursor, ChatGPT | `CURSOR_HOME`, `CHATGPT_HOME` | none exists — these tools offer no relocation variable |

A few names look like they should work but do not, because the real tool means
something else by them:

- **`KIRO_HOME` moves only half of Kiro.** Kiro ships two products under one
  `~/.kiro`, and only one of them has a relocation variable. `kiro-cli` reads
  `KIRO_HOME` and, when it is set and non-empty, uses it as the whole root
  (`.kiro` is *not* appended), so its sessions move to
  `$KIRO_HOME/sessions/cli/`. The Kiro **IDE** has no such variable at all —
  `KIRO_HOME` does not appear anywhere in the shipped desktop package, and every
  place the bundled agent extension builds its store path uses `os.homedir()`.
  So with `KIRO_HOME` set, casr reads CLI sessions from there and IDE sessions
  from `~/.kiro/sessions/` — each store from where its own product actually
  writes. Moving `HOME` moves both, because both ultimately resolve the home
  directory. One further real variable is **not** read:
  `KIRO_TEST_SESSIONS_DIR` replaces `sessions/cli` outright in `kiro-cli`, but
  it belongs to that binary's `KIRO_TEST_*` test harness rather than to users.
- **`AMP_HOME` is not read.** It is Amp's own variable, but it relocates Amp's
  *install* directory (the tree holding `bin/`), which never contains threads.
  `XDG_DATA_HOME` is the only variable that moves Amp's data.
  `AMP_DATA_HOME` is read by nothing casr can find — zero occurrences across the
  CLI binary and six shipped extension builds — so it is not read either.
- **The Amp store casr reads is the editor extension's, not the CLI's.** Both
  products share `<XDG_DATA_HOME>/amp`, but the CLI keeps thread bodies
  server-side and writes only `daemon/`, `ide/`, `oauth/`, `runner/`,
  `notepad/`, `device-id.json`, `history.jsonl`, `session.json` and
  `secrets.json` there. `threads/` is written by `sourcegraph.amp`. On a machine
  with the CLI and no extension, an empty Amp list is the correct answer.
- **Aider has no home directory to point at.** It keeps one
  `.aider.chat.history.md` per *repository*, at the git work-tree root, found by
  walking up from wherever you started it (`aider/main.py` uses
  `git.Repo(search_parent_directories=True)`); with no repository at all it
  falls back to `./.aider.chat.history.md`. casr resolves the file the same way,
  so it sees the same sessions aider does from anywhere inside a checkout.
  `AIDER_CHAT_HISTORY_FILE` is aider's own override of that path — aider's
  parser derives it from `--chat-history-file` via `auto_env_var_prefix="AIDER_"`
  — and names one exact file. `AIDER_HOME` is casr's alone; aider never reads
  it. It points casr at a tree of checkouts to scan, and is also where casr
  writes converted sessions.
- **`VIBE_HOME` is the `~/.vibe` root**, not the session-log directory; casr
  appends `logs/session` exactly as Vibe does.
- **`OPENCLAW_HOME` replaces `$HOME`**, so OpenClaw's state lives at
  `$OPENCLAW_HOME/.openclaw`. `OPENCLAW_STATE_DIR` names that state directory
  outright and outranks it. Sessions are keyed by agent —
  `<state>/agents/<agent-id>/sessions/` — and casr reads every agent's
  directory. Target imports use the authenticated Gateway and do not mutate
  this shared store directly.

## Canonical Session Model

Core model (conceptual):

```text
CanonicalSession
  - session_id: String
  - provider_slug: String
  - workspace: Option<PathBuf>
  - title: Option<String>
  - started_at: Option<epoch_millis>
  - ended_at: Option<epoch_millis>
  - messages: Vec<CanonicalMessage>
  - metadata: serde_json::Value
  - source_path: PathBuf
  - model_name: Option<String>

CanonicalMessage
  - idx: usize
  - role: User | Assistant | Tool | System | Other(String)
  - content: String
  - timestamp: Option<epoch_millis>
  - author: Option<String>
  - tool_calls: Vec<ToolCall>
  - tool_results: Vec<ToolResult>
  - extra: serde_json::Value
```

Important helpers:
- `flatten_content`: normalizes mixed string/block content representations.
- `parse_timestamp`: normalizes ISO strings, epoch seconds, and epoch millis.
- `normalize_role`: maps provider-specific roles to canonical roles.
- `reindex_messages`: keeps message indices contiguous after filtering.

## Architecture

```text
Input CLI
  casr <target> resume <session-id>
          |
          v
Provider Registry + Detection
  - discover installed providers
  - optional --source narrowing
          |
          v
Session Discovery
  - find owning provider + source path
          |
          v
Reader (Provider-specific native format -> CanonicalSession)
  Claude/Codex/Gemini/etc.
          |
          v
Validation
  - hard errors: empty / one-sided sessions
  - warnings/info: missing workspace, timestamp gaps, metadata loss
          |
          v
Writer (CanonicalSession -> target native format)
  - generate target session id
  - preserve provider-specific extras when possible
          |
          v
Atomic Write + Conflict Handling
  - temp file -> fsync -> rename
  - optional --force backup (.bak)
          |
          v
Read-Back Verification
  - re-read written session via target reader
  - compare structural fidelity
          |
          v
Output
  - human output with actionable steps
  - optional JSON output for automation
```

## Why This Is Useful in Day-to-Day Work

`casr` is built for practical agent handoff problems, not only format conversion demos.

- You can switch models mid-task without rebuilding context from scratch.
- You can recover from provider outages or rate limits by moving the same session to another CLI.
- You can keep one durable transcript while changing agent personas and tool stacks.
- You can move a session into the provider that has the strongest tooling for the next step, then move back.

Common examples:
- Start in Codex for rapid code edits, then resume in Claude Code for architecture review.
- Start in Gemini for long context analysis, then resume in Codex for implementation.
- Recover old sessions from one provider and continue them in another after a tooling migration.

## CLI Ergonomics and Alias Normalization

`casr` supports two equivalent resume styles:

- Canonical subcommand form: `casr <target> resume <session-id>`
- Shorthand form: `casr -cc <session-id>`, `casr -cod <session-id>`, `casr -agy <session-id>`, `casr -gmi <session-id>`

Shorthand flags are rewritten internally before clap parsing, so logging, JSON output, and error handling stay identical across both forms.

Alias normalization also accepts common provider tokens:

- `claude` maps to `claude-code`
- `codex-cli` maps to `codex`
- `gemini-cli` maps to `gemini`

## Deterministic Resolution Algorithm

The resolver is intentionally strict and deterministic.

1. If `--source` parses as a path, `casr` bypasses provider scanning and resolves from that path.
2. If `--source` parses as an alias, `casr` searches only that provider.
3. If no source hint is provided, `casr` scans installed providers and collects all matches.
4. Zero matches returns `SessionNotFound`.
5. One match proceeds.
6. Multiple matches returns `AmbiguousSessionId` and includes candidates.

Path mode has additional fallback logic when a file is outside known provider roots:

1. Try extension and file-signature heuristics.
2. If heuristics fail, ask each provider parser to read the file.
3. If any successful parse is plausible, discard non-plausible successes;
   otherwise retain every non-empty success.
4. Accept the parser only when exactly one candidate remains. Multiple
   candidates return `AmbiguousSessionId`; message count and provider name
   never break the tie because neither proves ownership.

Plausibility currently requires at least one user message and one assistant message.

## Detailed Pipeline Contract

The conversion pipeline in `src/pipeline.rs` has a fixed stage order:

1. Resolve target provider from alias.
2. Resolve source session.
3. Read source into canonical IR.
4. Validate canonical session.
5. Optionally prepend synthetic enrichment context (`--enrich`).
6. Short-circuit on `--dry-run`.
7. Short-circuit same-provider conversion when enrichment is not requested.
8. Write target-native session.
9. Re-read written output and verify structural fidelity.

If read-back verification fails, `casr` rolls back written files and restores backups when available. This keeps failed conversions from leaving unverified artifacts in target storage.

## Core Normalization Algorithms

### Content normalization (`flatten_content`)

`casr` accepts several message content shapes and normalizes them into canonical text:

- Plain strings.
- Arrays of text blocks.
- Arrays of Codex-style `input_text` blocks.
- Tool-use blocks with fallback textual descriptions.
- Objects containing `text` or ChatGPT-style `parts`.

This allows each provider adapter to keep format-specific parsing small while still converging on one canonical message representation.

### Timestamp normalization (`parse_timestamp`)

The parser accepts:

- Integer epoch seconds and epoch milliseconds.
- Floating-point seconds.
- Numeric strings.
- RFC3339 and common ISO-8601 formats.

Heuristic detail: values below `100_000_000_000` are treated as seconds; larger values are treated as milliseconds.

### Role normalization and verification buckets

Roles are normalized to `User`, `Assistant`, `Tool`, `System`, or `Other(String)`.
Read-back verification compares role buckets rather than raw role enums for known lossy formats. For example, providers that collapse non-assistant roles into a single user-like entry type still pass verification when semantic intent is preserved.

## Atomic Write and Recovery Semantics

File-backed `casr` write operations are temp-then-rename and include rollback
behavior:

1. Create parent directories if needed.
2. If target exists and `--force` is not set, return conflict.
3. If target exists and `--force` is set, rename target to a deduplicated backup (`.bak`, `.bak.1`, and so on).
4. Write full content to temp file in the same directory.
5. Flush and `sync_all` temp file.
6. Rename temp file to target path.

If any step fails:

- Temp files are cleaned up.
- Existing backups are restored to original target paths.
- Errors include provider and path context for debugging.

Providers with vendor-owned shared stores override this lifecycle. OpenCode
imports and deletes sessions through the official CLI, so verification failure
removes only the imported session and never rolls back the whole database.

## `casr list` Selection and Ranking Internals

The `list` command is optimized for project-local triage first.

- Default scope is the current working directory project.
- `--workspace` can override scope explicitly.
- Provider-specific path hints are used for fast filtering (`claude-code`, `gemini`).
  A hint answers *matches*, *differs*, or *unknown*; only *differs* excludes a
  session, because "this layout encodes no workspace" is not evidence.
- Sessions no source can place in any workspace — the four providers that never
  record one (`vibe`, `chatgpt`, `clawdbot`, `antigravity`), plus any session of
  the others whose file carries no `cwd` — are **listed** under the default
  scope and **hidden with a warning on stderr** under an explicit `--workspace`.
  The default scope is a convenience, not a question the user asked;
  `--workspace X` is a question, and a session nobody can place in X is not an
  answer to it. `list --json` reports `"workspace": null` for these either way.
- Providers that support `list_sessions()` can bypass expensive filesystem walks.
- Fallback directory scans are capped by depth, and filtered by the provider's
  own rule for what its tool writes (`is_session_path()`) rather than by a
  blanket extension list. ClawdBot's `sessions.json`, Factory's
  `<sessionId>.settings.json` and Vibe's `meta.json` all sit in a session
  directory and are not sessions; a listing that shows them as sessions with
  zero messages is wrong in a way a user cannot see.
- A candidate the provider's reader **cannot parse**, and a directory the
  listing **could not read**, are neither of them silently dropped. They are
  missing from the table — one unreadable file must not abort the whole listing
  — but the run reports how many were skipped, per provider, with up to three
  paths and the reason on stderr. `list --json` carries every one of them in
  `skipped: [{provider, path, error}]`, always present and `[]` on a clean run,
  because a short listing and a complete one are otherwise the same document.
  A store directory that simply does not exist yet is not one of these: that is
  the ordinary state of an installed tool that has not been run, and `casr
  providers` names the directory and says so instead.

When sorting by date, probe size is capped to avoid slow scans:

- Workspace-scoped mode uses `max(limit * 3, 30)`.
- Global mode uses `max(limit * 8, 30)`.

`Last Active` is computed from canonical conversation activity and file modification time, then rendered as relative age.

## Performance and Scaling Notes

- Resolution without a source hint is `O(number_of_installed_providers)` for ownership checks.
- Path fallback parsing runs only when root-based ownership and signatures are inconclusive.
- Listing can still be I/O-heavy on very large session trees, but probe caps and provider-native listing APIs keep it bounded in normal use.
- Providers that store many sessions inside one DB/file can implement `list_sessions()` for efficient enumeration and better counts.

## Design Principles Behind the Implementation

- Deterministic behavior over clever heuristics.
- Fail safely with explicit errors and rollback.
- Preserve session content first; preserve provider metadata when practical.
- Prefer additive warnings over hard failure when a conversion is still useful.
- Keep provider adapters independent behind one trait so new providers do not require pipeline rewrites.

## Adding a New Provider

To add a provider, implement the `Provider` trait in `src/providers/<provider>.rs`:

- `detect()`: installation probe with useful evidence strings.
- `session_roots()` and `owns_session()`: discovery hooks.
- `read_session()`: native format to canonical model.
- `write_session()`: canonical model to native format.
- `resume_command()`: exact command users should run after conversion.
- `list_sessions()` (optional): multi-session enumeration for providers that can
  do better than a directory walk. Returns a `SessionListing` carrying both the
  sessions and every place the enumeration was refused; `None` means "this
  provider does not enumerate itself", never "the enumeration failed".
- `is_session_path()`: the tool's own rule for which files under
  `session_roots()` are sessions. Take it from the shipped artifact, not from
  documentation, and never write it as a list of names to exclude.

Recommended test set for new providers:

- Reader and writer unit tests for native fixtures.
- Round-trip tests (`read(write(read(...)))`).
- CLI integration test path through `casr list`, `casr info`, and `casr <target> resume`.
- Error-path tests for malformed input and file I/O failures.

## Provider Format Notes

### Claude Code
- Source path pattern: `~/.claude/projects/<project-hash>/<session-id>.jsonl`
- JSONL events: `user`, `assistant`, and other event types (skipped when non-message)
- Writer emits provider-plausible JSONL with expected fields and timestamps.

### Codex
- Source path pattern: `~/.codex/sessions/YYYY/MM/DD/rollout-N.jsonl`
- JSONL events include `session_meta`, `response_item`, and `event_msg` variants.
- Writer emits `session_meta` and response events plus token-count events when available.

### Gemini CLI
- Source path pattern: `~/.gemini/tmp/<hash>/chats/session-<id>.json`
- JSON includes `sessionId`, `projectHash`, `messages`, and temporal fields.
- Writer emits `user` and `gemini` message types with provider-compatible structure.

### Antigravity
- Source/target database:
  `~/.gemini/antigravity-cli/conversations/<conversation-id>.db`
- Readable transcript:
  `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`
- Target writes delegate trajectory creation and resume verification to
  `google-antigravity>=0.1.9`; casr does not synthesize Antigravity protobuf
  blobs or edit a shared SQLite database.
- Install the optional target dependency with
  `python3 -m pip install "google-antigravity>=0.1.9"`.

### Cursor
- Source path pattern: `~/.config/Cursor/User/globalStorage/state.vscdb`
- SQLite `cursorDiskKV` keys: `composerData:<id>` and `bubbleId:<composerId>:<bubbleId>`.
- `casr` uses a virtual per-session path (`state.vscdb/<encoded-session-id>`) for deterministic lookup and verification.
- Cursor is read/resume-only: the editor also needs its `allComposers` index, whose safe write lifecycle is not yet verified.

### Aider
- Source path pattern: `.aider.chat.history.md` at the repository root (or the path named by `AIDER_CHAT_HISTORY_FILE`)
- Target writes use a dedicated native history file and pass it through
  `--chat-history-file <path> --restore-chat-history`; the shared append-only
  history is never modified.
- Aider's official restore parser drops blockquote turns. casr uses blockquotes
  only as non-model-visible same-role boundaries and folds non-assistant roles
  into visible user turns, which is reported as a fidelity loss.

## Validation Rules

Hard-stop errors:
- No messages.
- Missing either user or assistant messages.

Warnings (conversion continues):
- Missing workspace.
- Missing timestamps.
- Unusual role ordering.
- Very short sessions.
- High malformed-line skip ratio.

Verbose info:
- Tool-call/result mismatch notes.
- Metadata-loss notes.

## Round-Trip and Fidelity Guarantees

Core invariant for each provider `P`:

```text
read_P(write_P(canonical)) ~= canonical
```

Cross-provider invariant:

```text
read_target(write_target(read_source(input))) preserves
  - message order
  - message role intent
  - message text content
  - timestamps (within normalization tolerance)
```

Known expected differences:
- New target session ID is generated.
- Some provider-specific metadata may not map one-to-one.
- Workspace extraction for some providers may be best-effort.

## Testing

### Unit and Integration

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### End-to-End

```bash
bash scripts/e2e_test.sh
```

### Opt-In Real-Provider Smoke Harness

```bash
bash scripts/real_provider_smoke.sh
```

Notes:
- Uses real provider CLIs and real provider homes (`CLAUDE_HOME`, `CODEX_HOME`, `GEMINI_HOME`, `CURSOR_HOME`).
- Explicitly reports `PASS`/`FAIL`/`SKIP` for each core path: `CC<->Codex`, `CC<->Gemini`, `Codex<->Gemini`.
- Writes detailed artifacts (command transcript, per-path stdout/stderr, matrix) under `artifacts/real-smoke/<timestamp>/`.

Test suite coverage includes:
- Reader and writer tests for all provider adapters.
- Canonical model helper tests (`flatten_content`, `parse_timestamp`, etc.).
- Conversion pipeline tests with mock providers.
- Cross-provider round-trip fidelity matrix tests.
- CLI integration tests with fixture-backed temp directories.
- Full shell-level e2e conversion paths and error scenarios.

## Troubleshooting

### "Session not found"

```bash
casr list
casr info <session-id>
casr cc resume <session-id> --source cod
```

### "Target provider not installed"

Check provider availability:

```bash
casr providers
```

Install the missing provider, then retry.

### "Session already exists in target"

Use force mode to back up and overwrite:

```bash
casr cc resume <session-id> --force
```

### "Write verification failed"

Run in trace mode and inspect JSON diagnostics:

```bash
casr cc resume <session-id> --trace --json
```

### "Wrong source provider was detected"

Pin source provider or session path explicitly:

```bash
casr cc resume <session-id> --source cod
casr cc resume <session-id> --source ~/.codex/sessions/2026/02/06/rollout-1.jsonl
```

## Limitations

- Provider-specific metadata cannot always be preserved perfectly across all provider pairs.
- Provider internal format changes can require reader/writer updates.
- Some workspace extraction paths are heuristic-based (especially when source format lacks explicit workspace).
- Resume acceptance depends on external provider behavior and may vary by provider version.

## Editor / Terminal Integrations

Community-built shortcuts that wrap `casr` for one-keystroke session forking:

- **iTerm2 (macOS)** — [pirate/iterm-agent-fork](https://github.com/pirate/iterm-agent-fork): native iTerm hotkey to fork the active session into a different coding agent via `casr`.

These are external projects, not maintained here. If you've built a similar integration and want it linked here, file an issue with the URL — see the [Contributions](#about-contributions) policy below.

## FAQ

### Is casr only for one-way migration?

No. It supports both directions where both providers expose verified native
write paths; read/resume-only providers remain valid sources.

### Does casr modify my source session?

No. It reads source sessions and writes only to a supported target provider's
storage.

### What happens when target session file already exists?

By default it stops with a conflict error. With `--force`, it creates a `.bak` backup and overwrites.

### Can I script casr in CI or automation?

Yes. Use `--json` output and non-interactive command patterns.

### How do I debug a failed conversion?

Use `--verbose` or `--trace`, optionally with `RUST_LOG=casr=trace`.

### Can I convert within the same provider?

Yes. Same-provider conversion is handled gracefully and may return a direct resume path/no-op behavior when appropriate.

## About Contributions

*About Contributions:* Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

MIT License (with OpenAI/Anthropic Rider). See [LICENSE](LICENSE).
