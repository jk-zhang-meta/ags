# Changelog

All notable changes to [casr](https://github.com/Dicklesworthstone/cross_agent_session_resumer) (Cross Agent Session Resumer) are documented here.

Versions correspond to [GitHub Releases](https://github.com/Dicklesworthstone/cross_agent_session_resumer/releases) unless marked **Unreleased**. Both releases have accompanying git tags; the distinction between "tag" and "release" is noted per version. Where a GitHub Issue motivated a change, it is linked inline.

---

## [Unreleased] (after v0.1.1)

> Commits on `main` since the v0.1.1 tag (`be1ce19`, 2026-03-03). No GitHub Release yet.

### AGS Runtime

- **The launcher menu detects an Agent the same way it launches one**: the menu
  asked `command -v codex` while `resolve_agent_binary` accepts codext, so a
  codext-only host could run `ags codex` yet be told no Codex was installed.
  Detection now goes through that same resolution (`agent_available`), and the
  installer's provider scan accepts codext too.

  `ags codex-name` still exists and still hands codext the name `codex` at
  install time, but nothing depends on it being run any more — that step is now
  about who owns a name, not about being visible at all.

- **codext stays bundled.** Installing AGS still installs it, `ags update` still
  carries it, and `ags codext-update` is still the command that brings it to the
  current release. codext is Codex with the credential pool wired in and AGS
  launches it in preference to stock Codex whenever it is on PATH, so an install
  that skipped it would leave every session quietly running on the machine's own
  account. It rides its own release train — the fork tracks upstream Codex,
  casr tracks AGS — which is why it is a separate version check rather than a
  branch of the casr update.

  (Only context-mode was unbundled. An earlier commit took codext out along with
  it; this restores it.)

- **Context Mode is no longer bundled**: installing AGS no longer installs,
  provisions, verifies, or updates `context-mode`, and launching Claude or Codex
  no longer requires it. The mandatory integration is gone in full — the
  installer's preflight and initialization step, the ~4,700-line provisioning
  runtime (npm resolution, staged activation, manifest verification, plugin and
  marketplace registration, hook trust, rollback), the `context-init` /
  `context-check` / `context review-codex` subcommands, and the launch-time gate
  in the Rust entry point. The install transaction journal drops its `context`
  block and moves to `schema: 2`; a journal written by an older installer is
  therefore not resumed. Anyone who wants Context Mode installs it themselves.

  The Agent argument restrictions stay, because they never really were about
  Context Mode: `--safe-mode`, `--agent`, `--mcp-config`, `--plugin-dir` and the
  rest can disable the AGS plugin the launch itself depends on. Only their
  wording changed. `preflight_installer_tools` now checks `jq`, Node, and
  sha256 on the installer's own behalf — that check used to ride along on
  Context Mode's preflight and would have disappeared with it.

- **The installer adopts wrappers written before the `agsx` rename**: ownership
  of `bin/ags` is decided by a marker comment, and `# ags-installer-` is not a
  substring of the older `# agsx-installer-`. Every machine installed before
  that rename therefore read as hand-written, was preserved, and would never
  have received a wrapper update again. Both markers are now recognised, and an
  actually hand-written wrapper is still left alone.
- **`ags update` says `ags`**: it reported `casr`, the upstream binary name,
  beside lines naming `codext` and `Context Mode` — an implementation detail in
  a line about a command nobody types `casr` to run. Only the messages changed:
  the binary, the release asset names, and the cosign identity `install.sh`
  pins are untouched.
- **Resuming asks which directory to work in**: a checkpoint records where its
  session was working, and `ags resume` used to restore into that path or fail
  when it was gone — which made a synchronized checkpoint unresumable anywhere
  but the machine that wrote it, because the saved path rarely exists on the
  next one. When the recorded directory and the directory you are resuming from
  are different absolute paths, both are now shown and you choose. When they are
  the same path nothing is printed. `--cwd PATH` answers in advance, and without
  a terminal to ask on the recorded directory is still used, so scripts keep the
  behaviour they were written against.
- **Resuming is quiet on a terminal**: `ags resume` inherited `ags restore`'s
  fourteen-field `key=value` record and printed it directly above the Agent's
  own banner. The record is a real output contract — scripts and the test suite
  read those fields — so it survives wherever stdout is not a terminal, and is
  dropped for the person who just picked a session. Warnings still reach stderr
  through `log` either way. Replayed Agent arguments are announced only when
  they were replayed rather than chosen.
- **Checkpoints remember their Agent arguments**: a session started as
  `ags codex --dangerously-bypass-approvals-and-sandbox` is saved with that
  flag and resumes under it, instead of silently coming back with different
  permissions than it was working under. The launch exports the argv it was
  given, `ags save` records it in the manifest as a JSON array, and
  `ags resume` replays it — `--` overrides, and `--` with nothing after it
  means "no arguments". Replayed arguments face the same Context Mode
  validation a typed one does, and are printed before the Agent starts, because
  these records synchronize between machines. Decoding is never `eval`, and the
  manifest `format` is deliberately not bumped so an AGS that has not updated
  yet still reads these checkpoints.
- **`ags list` is a picker, not a dump**: it renders through the same
  width-aware layout the piped table uses, so Chinese descriptions no longer
  leave the columns ragged, scrolls within the frame when there are more
  sessions than terminal rows, and shows the highlighted session's Agent
  arguments with `a` to edit them for this launch. `j`/`k` and `q` join the
  arrow keys and Esc.
- **Mandatory managed terminals**: `ags`, `ags claude`, `ags codex`, and
  checkpoint resume now use a private RMUX server for detachable live PTYs.
  Agent argv and environment overrides cross RMUX only in the structured
  requesting-client payload, never in RMUX's process-list-visible command
  vector.
  The installer provisions the verified official RMUX 0.9.1
  client/daemon/helper layout with crash-recoverable activation, preserves a
  complete compatible stable 0.9.x installation, and never signals an existing
  daemon.
- **Unified checkpoint storage**: local, named Git/GitHub, and
  SFTP/Neburst modes now have MRU selection, pre-launch reconciliation,
  automatic save/delete synchronization with durable retry, reversible
  content-verified merge redirects, and recoverable Git/SFTP retirement.
  Retry synchronization and acknowledgement are one serialized transaction, so
  a concurrent failed update cannot lose its replacement retry. Legacy cloud
  deletion is exact-record and interruption safe: a published tombstone can
  resume cleanup without deleting a newer checkpoint that reused the same
  logical ID, and the active encrypted archive remains selectable until every
  optional sidecar has moved to recoverable trash.
  Legacy Neburst cloud archives are imported into the unified content revision;
  password transports use encrypted generation files, obscured rclone
  environment values, and inherited SSH file descriptors.
- **Mandatory Context Mode integration**: the installer now provisions the
  newest AGS-compatible stable `context-mode` release from npm's official
  `latest` channel. Candidates are exact-version/integrity checked, staged
  without lifecycle scripts, health-tested, and atomically activated; a failed
  update restores the previous provider bindings and last-good runtime.
  `ags init` converges official Claude/Codex plugin registrations, and every
  managed Agent launch fails closed if the plugin or Codex hooks feature is unavailable. Online
  initialization runs upstream `doctor`; offline initialization proves a
  persistent SQLite/FTS round trip with public `index`/`search` commands.
  npm integrity is supplemented by a SHA-256 manifest over the complete
  runtime package tree and exact Agent plugin-cache comparisons. Runtimes are keyed by
  platform, architecture, and Node native ABI, so ABI changes are rebuilt
  online without weakening offline validation. Claude launches additionally
  prove the effective hook, MCP, tool, and plugin state in a short-lived real
  process and reject session-only plugin/tool overrides. Installer and
  initialization transactions use durable recovery journals and roll back
  AGS-owned changes or the prior binary after interruption. Codex
  launches additionally require all six exact Context hooks to report trusted
  through the official app-server API; `ags context review-codex` opens the
  official review UI, and AGS never bypasses or synthesizes trust.

### New Providers

- **Grok Build (initially read-only)**: new `grok` provider (alias `grk`) for xAI's official `grok` CLI ([#19](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/19)). Reads the ACP session-update stream (`updates.jsonl`) plus `summary.json` metadata under `$GROK_HOME/sessions/<percent-encoded-cwd>/<session-uuid>/`, coalescing streamed message/thought chunks and merging `tool_call`/`tool_call_update` events; unknown update kinds are skipped tolerantly. The provider initially returned an explanatory error for target writes pending round-trip verification against a live `grok --resume <session-id>`.

### Provider Write Support

- **Antigravity**: target writes now use Google's official `google-antigravity>=0.1.9` SDK to create and reopen a native SQLite trajectory before handoff to `agy`. The import preserves complete user/model turns, visibly labels/coalesces roles the public SDK cannot inject directly, refuses unanswered trailing user turns, and rolls back failed imports.

### Structured Responses and Workspace Enrichment

- **Responses module with typed JSON envelope**: new `responses` module emitting versioned, typed JSON structs for CLI output, preventing schema drift between versions ([`435fd0a`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/435fd0a7ba2ca841831b0b881db9772cea538f0b), [`4b99a2f`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/4b99a2f53634b99748b1609063a69646038187cc)). Addresses proposals [#6](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/6) and [#8](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/8).
- **Git repo discovery**: automatic `.git` marker detection enriches session metadata with repository name and branch, improving workspace-aware output ([`03a68c0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/03a68c0f47871ecc9cd734816c476c4317a909db)). Addresses proposals [#5](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/5) and [#7](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/7).

### Cross-Provider Conversion Fixes

- **Pi-Agent content serialization**: always emit `content` as a JSON array to prevent `TypeError` in Pi runtime (closes [#9](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/9)) ([`a805eb3`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/a805eb3143278a588b127da5744db04fd7b02af0)).
- **Codex-to-Pi conversion**: fix message count mismatch, content serialization format, and resume command to use `pi --session <path>` (closes [#9](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/9)) ([`5e58c9d`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5e58c9d6bb64584424e48608b1b9b0f42af014ee), [`8dd76f0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/8dd76f08d0b163e4833a8548d6302c1081c0a474)).
- **AMP-to-Codex conversion**: emit valid `session_meta` with `payload.timestamp` and correct content types so Codex CLI accepts the output (closes [#10](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/10)) ([`6152b9a`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/6152b9aafdcc156cac12931e6c005a44c9ecc50b)).

### Provider Compatibility

- **`developer` role mapping**: map the `developer` role to `System` so sessions with developer-authored system prompts round-trip correctly; also fixes Pi-Agent usage lookup ([`076c090`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/076c0902309be823005a6ff6843324324e12b5ff)).
- **Codex/Pi serialization fidelity**: improve serialization accuracy during conversion between Codex and Pi-Agent ([`03a68c0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/03a68c0f47871ecc9cd734816c476c4317a909db)).

---

## [v0.1.1] -- 2026-03-04

> **GitHub Release**: [v0.1.1](https://github.com/Dicklesworthstone/cross_agent_session_resumer/releases/tag/v0.1.1) -- published 2026-03-04 with pre-built binary assets.
> **Git tag**: lightweight tag on commit [`be1ce19`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/be1ce193a0b9512ce3b8417e8b5b1b4a7420d9d2).
> One commit on top of v0.1.0: version bump and license-field correction.

### Release Packaging

This release exists primarily to attach binary distribution artifacts. The single `release: prepare v0.1.1` commit ([`be1ce19`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/be1ce193a0b9512ce3b8417e8b5b1b4a7420d9d2)) updates `Cargo.toml` version from `0.1.0` to `0.1.1` and switches the license field from `license = "MIT"` to `license-file = "LICENSE"` for the OpenAI/Anthropic Rider.

### Binary Artifacts

| Asset | Description |
|---|---|
| `casr-x86_64-unknown-linux-musl.tar.xz` | Statically linked Linux x86_64 binary (55 downloads) |
| `casr-aarch64-apple-darwin.tar.xz` | macOS Apple Silicon binary (17 downloads) |
| `casr_darwin_arm64` | macOS arm64 bare binary |
| `casr` | Linux bare binary |
| `SHA256SUMS` | Checksum file for all artifacts |
| `cross_agent_session_resumer-v0.1.1-manifest.json` | Release manifest for the `curl\|bash` installer |

---

## [v0.1.0] -- 2026-03-04

> **GitHub Release**: [v0.1.0](https://github.com/Dicklesworthstone/cross_agent_session_resumer/releases/tag/v0.1.0) -- published 2026-03-04, source-only (no binary assets).
> **Git tag**: lightweight tag on commit [`e2329a1`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/e2329a17ecad8d8ec2369a687655d91ed3ab56ed).
> Covers the entire development history from project inception (2026-02-08) through the first tagged release (104 commits).

### Canonical Session Model

The core data model that makes cross-provider conversion possible:

- **`CanonicalSession` / `CanonicalMessage` IR**: provider-agnostic intermediate representation with session ID, workspace, timestamps, messages, tool calls/results, and extensible metadata ([`5a92330`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5a9233065ab3bfeb05de64d3f7aee63946655408)).
- **Content normalization** (`flatten_content`): handles plain strings, text-block arrays, Codex `input_text` blocks, tool-use blocks with fallback descriptions, and ChatGPT `parts`-style objects ([`5a92330`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5a9233065ab3bfeb05de64d3f7aee63946655408), [`36af9f0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/36af9f08f8ea6df7f6a2e4d3ca6680f41b93cc98)).
- **Timestamp normalization** (`parse_timestamp`): accepts epoch seconds, epoch milliseconds, floats, numeric strings, and RFC3339/ISO-8601; heuristic threshold at `100_000_000_000` distinguishes seconds from millis ([`5a92330`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5a9233065ab3bfeb05de64d3f7aee63946655408), [`8151748`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/815174805e77c785c8972dd9ddda466d54c3cc81)).
- **Role normalization**: maps provider-specific roles to `User`, `Assistant`, `Tool`, `System`, or `Other(String)` with bucket-based verification for lossy formats ([`5a92330`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5a9233065ab3bfeb05de64d3f7aee63946655408)).

### Conversion Pipeline

- **Fixed-stage pipeline**: `resolve` -> `read` -> `validate` -> `enrich` -> `write` -> `verify`, with atomic temp-then-rename writes, `.bak` backup on `--force`, and automatic rollback on verification failure ([`85d31b8`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/85d31b886b0212d04396253fdc309d1a9b2c2a04)).
- **Deterministic session resolver**: strict ownership probing across installed providers, optional `--source` narrowing, `AmbiguousSessionId` on multi-match, and file-path fallback with heuristic ranking by plausibility and message count ([`2d5718b`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/2d5718b6738a7d2f5e3e8d84bc37a614988c76bb)).
- **Read-back verification**: re-reads written output via target reader and compares structural fidelity; relaxed role-bucket comparison tolerates lossy round-trips such as `developer` -> `System` ([`5a65954`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5a659542de83c8f682ac757cfe41f3a2ef51c42e)). Addresses [#2](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/2).
- **Same-provider short-circuit**: graceful no-op behavior when source and target are the same provider and enrichment is not requested.

### Provider Support (14 providers, all with read + write)

All 14 providers were implemented via a pluggable `Provider` trait with `detect`, `session_roots`, `owns_session`, `read_session`, `write_session`, `resume_command`, and optional `list_sessions` methods ([`85d31b8`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/85d31b886b0212d04396253fdc309d1a9b2c2a04), [`8d03785`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/8d03785e871749cc887cac56f28707fcb3dd09e3)).

#### Core providers (JSONL/JSON file-based)

| Provider | Alias | Format | Key commit |
|---|---|---|---|
| Claude Code | `cc` | JSONL events (`~/.claude/projects/`) | [`85d31b8`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/85d31b886b0212d04396253fdc309d1a9b2c2a04) |
| Codex | `cod` | JSONL rollout (`~/.codex/sessions/`) | [`85d31b8`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/85d31b886b0212d04396253fdc309d1a9b2c2a04) |
| Gemini CLI | `gmi` | JSON (`~/.gemini/tmp/`) | [`85d31b8`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/85d31b886b0212d04396253fdc309d1a9b2c2a04) |

#### IDE-integrated providers (SQLite / VS Code storage)

| Provider | Alias | Format | Key commit |
|---|---|---|---|
| Cursor | `cur` | SQLite `state.vscdb` with virtual per-session paths | [`579321a`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/579321a3aa44293b5cb5048478d390fb5f986e4b) |
| Cline | `cln` | VS Code globalStorage JSON | [`8328426`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/83284262feda2bb0abba51a84c2108413e0b6049) |
| OpenCode | `opc` | SQLite-backed session store | [`11a3709`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/11a3709401ecb92ab6f2de3a0231214f42bb8074) |

#### CLI/terminal agent providers

| Provider | Alias | Format | Key commit |
|---|---|---|---|
| Aider | `aid` | Markdown chat history | [`7d1d4c6`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/7d1d4c626b505e3238ab35aad3483135863e9d6c) |
| Amp | `amp` | Thread JSON | [`8328426`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/83284262feda2bb0abba51a84c2108413e0b6049) |
| ChatGPT | `gpt` | Mapping-tree and simple-messages JSON | [`7a68f05`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/7a68f05be107846b461adf6c195e4f9997d2e641) |

#### CASS-parity providers (completing the provider matrix)

| Provider | Alias | Format | Key commit |
|---|---|---|---|
| ClawdBot | `cwb` | JSONL messages | [`6f52872`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/6f52872ad259927bfe15095125585e785b99496a) |
| Vibe | `vib` | JSONL with flexible parsing | [`b8cd166`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/b8cd166c37acc7dbcfd93e77fee52b48391e80d3) |
| Factory | `fac` | JSONL with flexible parsing | [`b8cd166`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/b8cd166c37acc7dbcfd93e77fee52b48391e80d3) |
| OpenClaw | `ocl` | JSONL | [`8b5b102`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/8b5b102a649353a568d5c23725b0715d50fa99c1) |
| Pi-Agent | `pi` | JSONL | [`8b5b102`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/8b5b102a649353a568d5c23725b0715d50fa99c1) |

### CLI and UX

- **Subcommand and shorthand resume**: both `casr <target> resume <id>` and `casr -cc <id>` / `casr -cod <id>` / `casr -gmi <id>` forms supported; shorthand flags rewritten internally before clap parsing ([`ebc7204`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/ebc72044a618ff72a41c11442c27067e9f2c3d8c)).
- **Rich terminal table for `casr list`**: styled columns for provider, session ID, workspace, message count, tool-use count, and relative last-active age ([`ebc7204`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/ebc72044a618ff72a41c11442c27067e9f2c3d8c), [`dc16517`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/dc16517ae34d9877f6c2bfb44a81b58bf59ff0d9)).
- **Workspace-scoped list**: `casr list` defaults to sessions from the current working directory; `--workspace` overrides explicitly ([`474e765`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/474e765f6cb2d7907e83c9aab631211930ed9442)).
- **Standard provider name aliases**: `claude` -> `claude-code`, `codex-cli` -> `codex`, `gemini-cli` -> `gemini` for natural command invocation ([`80c5789`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/80c5789d06770aca4d571ffe3e1cc070b58d7440)).
- **Shell completions**: `casr completions bash|zsh|fish` generates registration stubs ([`e022b9b`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/e022b9bc58d6a80aebb105428279c36aaadf6cde)).
- **Global flags**: `--dry-run`, `--force`, `--json`, `--verbose`, `--trace`, `--source <alias_or_path>`, `--enrich`.

### Session Listing Intelligence

- **Session metrics**: message count, tool-use count, and per-provider grouping in `casr list` output ([`faf955c`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/faf955c285afdc3cd787cc31cb337ef49ccb8c44)).
- **Parallel session parsing**: concurrent parsing with a configurable parallelism threshold to avoid thread overhead on small sets ([`edcc2c7`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/edcc2c72eaa91a23e071afa99e0da06a6c7b1282), [`faf955c`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/faf955c285afdc3cd787cc31cb337ef49ccb8c44)).
- **Last-active tracking**: semantic relative-age rendering computed from canonical conversation timestamps and file modification time ([`dc16517`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/dc16517ae34d9877f6c2bfb44a81b58bf59ff0d9), [`80c5789`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/80c5789d06770aca4d571ffe3e1cc070b58d7440)).
- **Tool-call/result extraction**: full extraction for Gemini (`functionCall`/`functionResponse`), Codex `output_text`, Claude Code tool-result serialization, and Factory/Codex/Gemini metric accuracy ([`f868918`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/f868918fffff37a77ff1960d8675ca66e913e6e9), [`4e474ff`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/4e474ffc73f4571dabe76b6060383b22cab1ff11)).
- **Canonical tool-use count preference**: list output uses canonical count over source-file fallback for accuracy ([`ff18dc3`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/ff18dc3fe0914953b4cf9eb760ffe74dfc33a368)).

### Installer

- **`curl | bash` installer** (`install.sh`): platform detection (Linux/macOS, x86_64/aarch64), SHA256 and Sigstore/cosign verification, download fallback chain (versioned release -> latest naming variants -> source build), `--offline <tarball>` airgap mode, proxy-aware networking, and shell completion installation ([`5b39095`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5b39095cf95579b55087c798fa8ed3bf24353541)).
- **Agent auto-configuration**: installs `casr` skill file for Claude Code and Codex, plus optional `cc`/`cod`/`gmi` wrapper scripts; `--no-configure` and `--no-skill` flags to opt out ([`dc4c9b0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/dc4c9b0a34e5faeb7ce54ce365a27235d1e880c1)).
- **Installer flags**: `--verify` (post-install self-test), `--force` (reinstall), `--from-source`, `--easy-mode` (PATH auto-update), `--yes` (non-interactive), `--system` (system-wide install).
- **Version extraction fix**: anchored regex for more robust version string parsing in the installer ([`690c5a3`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/690c5a3cf0aa882b4f5800d8e28c28d910a8a3b4)).

### Performance

- **BufReader streaming IO**: all provider readers switched from `read_to_string` to buffered streaming, reducing peak memory on large sessions ([`f086e91`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/f086e91074219948b76a00a8dd8eeb508d907c8e)).
- **Parallelism threshold**: avoids spawning threads for small provider sets during `casr list` ([`edcc2c7`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/edcc2c72eaa91a23e071afa99e0da06a6c7b1282)).

### Provider-Specific Bug Fixes

- **Codex deduplication**: remove mirrored user turns that appear in some Codex JSONL session files ([`e97258b`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/e97258b10b14a47b2f57445b1267a2a94ee20f60)).
- **Cursor/OpenCode transactional writes**: wrap `write_session` SQLite operations in transactions for atomicity ([`b0c3658`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/b0c3658feab69310eace8252694e8ff5b65e8846)).
- **Aider writer correctness**: stream history files instead of buffering, preserve existing file content on write, fix workspace extraction ([`28fb49f`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/28fb49f44bbd477dd8e2b55081d19423175c0d8a)).
- **Factory workspace slug**: correct decoding for workspace names with a leading dash ([`a9043ec`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/a9043ece9c5b64d3fb37848f723a7754705154c1)).
- **OpenClaw detection**: improve detection heuristics and Pi-Agent filename/metadata handling ([`ec35a63`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/ec35a63573d9742b2555c937d27c1b3cf767dbc7)).

### Pipeline and Discovery Fixes

- **Backup path encoding**: use `OsString`-based path construction to avoid encoding issues on non-UTF-8 paths ([`50f18e3`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/50f18e305faf6c5b3ee729684f01eb0987834799)).
- **JSONL detection hardening**: stricter file-signature heuristics and edge-case timestamp parsing ([`8151748`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/815174805e77c785c8972dd9ddda466d54c3cc81)).
- **Virtual `--source` paths**: support virtual source paths for providers like Cursor that use composed paths (`state.vscdb/<encoded-id>`), with stricter readback verification ([`2d5718b`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/2d5718b6738a7d2f5e3e8d84bc37a614988c76bb)).
- **Consecutive-role validation**: removed the warning for consecutive same-role messages, which was noise for valid multi-turn provider formats ([`c3c4e3c`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/c3c4e3c71e1c98f42b70371f2ccd854ca1526e76)).

### Testing

- **14x14 provider conversion matrix**: exhaustive cross-provider round-trip test coverage across all provider pairs ([`c940298`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/c94029877f6628ef9131391890d517a976c957f5)).
- **127-test end-to-end suite**: `scripts/e2e_test.sh` covering all 14 providers with JSON reporting, timing, and artifact generation ([`50316e5`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/50316e5e31964482a3336d69234e4200f0f04da2), [`f46b90d`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/f46b90d0f307ab888e7310c88e47c189b2331f56)).
- **32 malformed-input tolerance tests**: all 14 providers tested against corrupted, missing, and empty input ([`518ad40`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/518ad40d661e99307c01d26d569f99d54246266b), [`3206d92`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/3206d922a91ab6161676d117bd86a57ed0809e39)).
- **Real-provider smoke harness**: `scripts/real_provider_smoke.sh` covering CC, Codex, Gemini, Cursor, Cline, Aider, Amp, and OpenCode with per-path PASS/FAIL/SKIP reporting ([`e8faaa5`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/e8faaa5856fd6bff98e29e2b9811906e44cdc3fe)).
- **Real-world sanitized fixtures**: production-derived session data for 3-provider round-trip matrix tests ([`eeceef9`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/eeceef9cc25252027314023dd73ab37e528cb183)).
- **Atomic write and error-path tests**: corrupted SQLite handling, invalid session IDs, and permission-denied paths ([`2843ba0`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/2843ba0c22297ec04cdf0b644375856b4922a668), [`7ee14b9`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/7ee14b91416bf47d119a10c57658fab2160b099f)).
- **Tracing observability tests**: validate that `--verbose` and `--trace` flags produce expected log levels ([`7f1a361`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/7f1a36137979ca8c6aa31820cdddc6d250b24c2b)).
- **Large-session stress tests**: verify performance and correctness on high-message-count sessions ([`c7f8b50`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/c7f8b50e3993fe15cfc274390bd5d7b3543e7d62)).

### CI Pipeline

- **Integration, roundtrip, and e2e jobs**: GitHub Actions workflow with separate test tiers ([`8bc66ff`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/8bc66ff7bd027eff0522ef1af58ada669e6cf090)).
- **Performance regression gates**: CI checks for conversion throughput regressions ([`f4fbb2a`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/f4fbb2a23e65601ab30b75f61623aacfdd9b4b99)).
- **Machine-readable test reports**: JSON artifact output for CI consumption ([`5d3b4d5`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/5d3b4d598063c4fdae44c9a437f7a54438e59ffc)).

### Housekeeping

- **License**: MIT with OpenAI/Anthropic Rider ([`29356d5`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/29356d54751dcb30a061b22f4b1e0acf1931a644), [`47f753e`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/47f753ec3f7baf97df762e36da7fb370173cf32a)).
- **Repo rename**: from `cross_agent_sessions_resumer` (plural) to `cross_agent_session_resumer` ([`af2d6ef`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/af2d6ef97cefaf4bb22c4e21400b266e87bf7f7e)).
- **GitHub social preview**: 1280x640 image added ([`e02c473`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/e02c47391763615627a9d872347ad324465edd3e)).

---

## Project Inception -- 2026-02-08

> First commit: [`068992b`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/068992bd3370da15d00212b720ae85460c2b2051) (initial planning and design docs).
> Rust project scaffolded the next day: [`1657cfe`](https://github.com/Dicklesworthstone/cross_agent_session_resumer/commit/1657cfea516e254cbcf442d76205ab4938047e8d).

---

## GitHub Issues Index

Issues that drove notable changes, linked to the versions where they were addressed:

| Issue | Title | Addressed in |
|---|---|---|
| [#2](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/2) | Codex-to-CC read-back verification role mismatch on `developer` messages | v0.1.0 |
| [#3](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/3) | Installer quirk as of `08b4091` | v0.1.1 |
| [#4](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/4) | `casr resume cod` produces JSONL that Codex CLI 0.107.0 cannot parse | v0.1.0 |
| [#5](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/5) | Workspace-aware metadata enrichment (proposal) | Unreleased |
| [#6](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/6) | Versioned JSON envelope for `list --json` (proposal) | Unreleased |
| [#7](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/7) | Move repo discovery out of model layer (proposal) | Unreleased |
| [#8](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/8) | Type JSON CLI responses with Serialize structs (proposal) | Unreleased |
| [#9](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/9) | Codex-to-Pi resumption failures (message count mismatch + TypeError) | Unreleased |
| [#10](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/10) | AMP-to-Codex invalid `session_meta` (missing `payload.timestamp`) | Unreleased |
| [#11](https://github.com/Dicklesworthstone/cross_agent_session_resumer/issues/11) | Support GitHub Copilot (feature request) | -- |

---

[Unreleased]: https://github.com/Dicklesworthstone/cross_agent_session_resumer/compare/v0.1.1...HEAD
[v0.1.1]: https://github.com/Dicklesworthstone/cross_agent_session_resumer/compare/v0.1.0...v0.1.1
[v0.1.0]: https://github.com/Dicklesworthstone/cross_agent_session_resumer/compare/068992b...v0.1.0
