//! How deep under a provider's session root a session is allowed to live.
//!
//! `casr list` walks each root recursively — `main.rs`, `max_depth(4)`, and the
//! same number in ClawdBot's and OpenClaw's own `list_sessions`. None of the
//! five providers here had a depth rule anywhere, so everything that survived
//! did so only because it happened to sit deeper than four levels. Attachments,
//! trashed threads, blob stores, per-task backups and subagent transcripts were
//! all inside the budget, and all of them were rendered as sessions or as
//! sessions that could not be read.
//!
//! Two different fixes, because there are two different bugs:
//!
//! * Amp, Cline and Vibe return `None` from `list_sessions`, so they go through
//!   the shared walk and the rule has to live in `is_session_path` — the shape
//!   `factory.rs` already uses.
//! * ClawdBot and OpenClaw already transcribe their tool's file-name rule
//!   exactly; what was wrong was the walk, so the fix there is `max_depth(1)`
//!   and nothing else.
//!
//! Every test drives the compiled binary and asserts on `list --json`, for the
//! same reason `list_truthfulness_test.rs` does: the envelope is the only place
//! that shows *both* halves of the symptom, a bogus row and a bogus "could not
//! be read" warning, and a leak can move between the two depending on whether
//! the decoy happens to parse.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// A `casr` invocation whose every provider home points inside `tmp`.
///
/// `XDG_DATA_HOME` matters twice over: it is Amp's store *and* casr's own
/// session store, so leaving it unset would have these tests create
/// `~/.local/share/ags` on the machine running them. `current_dir` is set for
/// the same class of reason: casr subcommands write relative to the cwd.
fn casr_cmd(tmp: &TempDir) -> Command {
    #[allow(deprecated)]
    let mut cmd = Command::cargo_bin("ags").expect("ags binary should be built");
    // 转换那套收在 `ags convert` 底下（`ags` 本身是会话运行时）。前缀加在这里
    // 而不是每个用例里：这个文件测的全是转换。
    cmd.arg("convert");
    cmd.env("CLAUDE_HOME", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("GEMINI_HOME", tmp.path().join("gemini"))
        .env("CURSOR_HOME", tmp.path().join("cursor"))
        .env("CURSOR_CONFIG_DIR", tmp.path().join("cursor-cli-config"))
        .env("CURSOR_DATA_DIR", tmp.path().join("cursor-cli-data"))
        .env("CLINE_HOME", tmp.path().join("cline"))
        .env("CLINE_DATA_DIR", tmp.path().join("cline-data"))
        .env("AIDER_HOME", tmp.path().join("aider"))
        .env("OPENCODE_HOME", tmp.path().join("opencode"))
        .env("CHATGPT_HOME", tmp.path().join("chatgpt"))
        .env("CLAWDBOT_HOME", tmp.path().join("clawdbot"))
        .env("CLAWDBOT_STATE_DIR", tmp.path().join("clawdbot-state"))
        .env("VIBE_HOME", tmp.path().join("vibe"))
        .env("FACTORY_HOME", tmp.path().join("factory"))
        .env("OPENCLAW_HOME", tmp.path().join("openclaw"))
        .env("OPENCLAW_STATE_DIR", tmp.path().join("openclaw-state"))
        .env("PI_AGENT_HOME", tmp.path().join("pi-agent"))
        .env("KIRO_HOME", tmp.path().join("kiro"))
        .env("GROK_HOME", tmp.path().join("grok"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("NO_COLOR", "1")
        .current_dir(tmp.path());
    cmd
}

/// Run `casr list --json` and return the parsed envelope.
fn list_json(tmp: &TempDir) -> serde_json::Value {
    let output = casr_cmd(tmp)
        .args(["list", "--json"])
        .output()
        .expect("casr list should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "list --json should emit an envelope: {e}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Paths one provider contributed to `key`, relative to `root` so the
/// assertions read like the store layout.
fn paths_under(
    envelope: &serde_json::Value,
    key: &str,
    provider: &str,
    root: &Path,
) -> Vec<String> {
    let mut out: Vec<String> = envelope[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` is an array"))
        .iter()
        .filter(|item| item["provider"].as_str() == Some(provider))
        .map(|item| {
            let path = item["path"].as_str().unwrap_or_default();
            Path::new(path)
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string())
        })
        .collect();
    out.sort();
    out
}

/// What the listing rendered as sessions, for one provider.
fn rows(envelope: &serde_json::Value, provider: &str, root: &Path) -> Vec<String> {
    paths_under(envelope, "items", provider, root)
}

/// What the listing reported as a session it could not read, for one provider.
/// This is the `⚠ N path(s) could not be read` line, itemised.
fn warnings(envelope: &serde_json::Value, provider: &str, root: &Path) -> Vec<String> {
    paths_under(envelope, "skipped", provider, root)
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir -p");
    fs::write(path, contents).expect("write fixture");
}

/// A one-message ClawdBot/OpenClaw transcript in `SessionManager` form.
fn pi_transcript(id: &str) -> String {
    format!(
        "{}\n{}\n",
        format_args!(
            r#"{{"type":"session","version":2,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z"}}"#
        ),
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#
    )
}

/// A one-message Amp thread, in the shape `Amp::build_thread_json` writes.
fn amp_thread(id: &str) -> String {
    format!(
        r#"{{"v":0,"id":"{id}","created":1767225600000,"title":"hello","messages":[{{"role":"user","content":"hello"}}]}}"#
    )
}

/// A one-message Cline API history.
const CLINE_API_HISTORY: &str = r#"[{"role":"user","content":[{"type":"text","text":"hello"}]}]"#;

// ---------------------------------------------------------------------------
// Amp — `!entry.isDirectory && name.endsWith(".json")` over one `readdir`
// ---------------------------------------------------------------------------

/// Amp's own listing rule, `keys()` in `sourcegraph.amp@0.0.1772799397`
/// (`extension/dist/extension.cjs`):
///
/// ```js
/// let H = await $.readdir(W);
/// let w = H.filter((B) => {
///     let N = B.uri.path.split("/").pop() || "";
///     return !B.isDirectory && N.endsWith(".json");
/// });
/// ```
///
/// One `readdir` of the threads directory, and `!B.isDirectory` is the proof
/// that it never descends: a directory entry is dropped rather than walked. So
/// a thread is a `.json` file *directly in* a threads root and nowhere else.
#[test]
fn amp_only_lists_threads_directly_in_a_threads_root() {
    let tmp = TempDir::new().expect("tempdir");
    let threads = tmp.path().join("xdg-data").join("amp").join("threads");

    write(
        &threads.join("T-11111111-1111-4111-8111-111111111111.json"),
        &amp_thread("T-11111111-1111-4111-8111-111111111111"),
    );

    // A file the user attached to a thread. Amp's `readdir` sees `attachments`
    // as a directory entry and drops it.
    write(
        &threads.join("attachments").join("att-1.json"),
        &amp_thread("att-1"),
    );
    // A deleted thread. Listing it puts a thread the user threw away back in
    // front of them.
    write(
        &threads
            .join(".trash")
            .join("T-22222222-2222-4222-8222-222222222222.json"),
        &amp_thread("T-22222222-2222-4222-8222-222222222222"),
    );
    // Content-addressed payload spilled out of a thread — two levels of
    // sharding, still inside the walk's four-level budget.
    write(
        &threads
            .join("blobs")
            .join("ab")
            .join("cd")
            .join("blob.json"),
        &amp_thread("blob"),
    );

    let envelope = list_json(&tmp);
    let listed = rows(&envelope, "amp", &threads);
    let skipped = warnings(&envelope, "amp", &threads);

    assert_eq!(
        listed,
        vec!["T-11111111-1111-4111-8111-111111111111.json"],
        "only a `.json` directly in the threads root is a thread; \
         got rows {listed:?} and warnings {skipped:?}"
    );
    assert!(
        skipped.is_empty(),
        "nothing under `attachments/`, `.trash/` or `blobs/` is a session Amp \
         failed to read — it is not a session at all; got {skipped:?}"
    );
}

// ---------------------------------------------------------------------------
// Cline — `.filter(isDirectory()).filter(/^\d+$/)` over one `readdir`
// ---------------------------------------------------------------------------

/// Cline's own task enumeration, from `saoudrizwan.claude-dev@4.0.11`
/// (`extension/dist/extension.js`):
///
/// ```js
/// (await readdir(t, { withFileTypes: true }))
///     .filter(n => n.isDirectory())
///     .map(n => n.name)
///     .filter(n => /^\d+$/.test(n))
/// ```
///
/// — one `readdir` of `tasks/`, keeping directories whose name is all digits.
/// A task is therefore `tasks/<digits>/`, exactly one level down, and casr's
/// own writer agrees: `generate_task_id` returns
/// `chrono::Utc::now().timestamp_millis().to_string()`.
///
/// The symptom here is not a bogus row. Cline's reader refuses anything whose
/// grandparent is not `tasks/` (`task_dir_from_api_path`), so each of these
/// came back as `⚠ path(s) could not be read` — a warning channel meant for
/// real failures, reporting files that were never sessions.
#[test]
fn cline_only_lists_tasks_one_level_under_the_tasks_root() {
    let tmp = TempDir::new().expect("tempdir");
    let tasks = tmp.path().join("cline").join("tasks");

    write(
        &tasks
            .join("1700000000000")
            .join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );

    // Too deep: the reader rejects each of these, so each became a warning.
    write(
        &tasks
            .join("1700000000000")
            .join("checkpoints")
            .join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );
    write(
        &tasks
            .join("1700000000000")
            .join("context_history")
            .join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );
    write(
        &tasks
            .join("backups")
            .join("a")
            .join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );
    write(
        &tasks
            .join("backups")
            .join("a")
            .join("b")
            .join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );

    // Right depth, wrong name: `/^\d+$/` is the other half of Cline's rule, and
    // this one the reader *accepts*, so it became a row with the task id
    // `backups`.
    write(
        &tasks.join("backups").join("api_conversation_history.json"),
        CLINE_API_HISTORY,
    );

    let envelope = list_json(&tmp);
    let listed = rows(&envelope, "cline", &tasks);
    let skipped = warnings(&envelope, "cline", &tasks);

    assert!(
        skipped.is_empty(),
        "a `.json` under a directory Cline never opens is not a session that \
         could not be read; got {} warning(s): {skipped:?}",
        skipped.len()
    );
    assert_eq!(
        listed,
        vec!["1700000000000/api_conversation_history.json"],
        "a task is `tasks/<digits>/`, one level down; got {listed:?}"
    );
}

// ---------------------------------------------------------------------------
// Vibe — `save_dir.glob(f"{prefix}_*")`, a single level
// ---------------------------------------------------------------------------

/// Vibe's own listing, `SessionLoader.list_sessions` in `mistral-vibe==2.22.0`
/// (`vibe/core/session/session_loader.py`):
///
/// ```python
/// pattern = f"{config.session_prefix}_*"
/// session_dirs = list(save_dir.glob(pattern))
/// ```
///
/// `glob`, not `rglob`, and no `**` in the pattern: one level under
/// `logs/session/` and no further.
///
/// What that excludes is not a synthetic decoy. `vibe/core/tools/builtins/
/// task.py` gives every subagent its own session logger rooted *inside* the
/// parent session:
///
/// ```python
/// session_logging = SessionLoggingConfig(
///     save_dir=str(ctx.session_dir / "agents") if ctx.session_dir else "",
///     session_prefix=args.agent,
///     ...
/// )
/// ```
///
/// so `session_<stamp>/agents/<agent>_<stamp>/messages.jsonl` is a real
/// transcript Vibe writes, two levels down, that Vibe itself never lists. casr
/// listed it as a peer of the session that spawned it.
///
#[test]
fn vibe_does_not_list_a_subagent_transcript_as_a_session() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = tmp.path().join("vibe").join("logs").join("session");
    let session_dir = sessions.join("session_20260101_000000_abc123");

    write(
        &session_dir.join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"hello vibe\"}\n",
    );
    write(
        &session_dir.join("meta.json"),
        r#"{
  "session_id": "abc123",
  "start_time": "2026-01-01T00:00:00+00:00",
  "end_time": null,
  "git_commit": null,
  "git_branch": null,
  "environment": {"working_directory": null},
  "username": "casr",
  "total_messages": 1
}
"#,
    );

    // A subagent spawned by the `task` tool.
    let subagent = session_dir
        .join("agents")
        .join("explorer_20260101_000100_def456");
    write(
        &subagent.join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"explore\"}\n",
    );
    write(
        &subagent.join("meta.json"),
        r#"{
  "session_id": "def456",
  "start_time": "2026-01-01T00:01:00+00:00",
  "end_time": null,
  "git_commit": null,
  "git_branch": null,
  "environment": {"working_directory": null},
  "username": "casr",
  "total_messages": 1
}
"#,
    );

    // Vibe's glob only sees direct `session_*` children. Its list oracle needs
    // a JSON-object transcript and a non-empty metadata session id; full
    // Pydantic metadata validation happens later, during resume.
    write(
        &sessions.join("raw-id").join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"raw directory\"}\n",
    );
    write(
        &sessions
            .join("session_20260101_000200_nometa")
            .join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"no metadata\"}\n",
    );
    write(
        &sessions
            .join("session_20260101_000300_badmeta")
            .join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"bad metadata\"}\n",
    );
    write(
        &sessions
            .join("session_20260101_000300_badmeta")
            .join("meta.json"),
        "{\"session_id\":\"\"}\n",
    );

    let envelope = list_json(&tmp);
    let listed = rows(&envelope, "vibe", &sessions);
    let skipped = warnings(&envelope, "vibe", &sessions);

    assert_eq!(
        listed,
        vec!["session_20260101_000000_abc123/messages.jsonl"],
        "a subagent transcript belongs to the session that spawned it, not \
         beside it; got rows {listed:?} and warnings {skipped:?}"
    );
}

// ---------------------------------------------------------------------------
// ClawdBot — the predicate was right, the walk was not
// ---------------------------------------------------------------------------

/// ClawdBot's own rule, `listSessionFilesForAgent` in `clawdbot@2026.1.24-3`
/// (`dist/memory/session-files.js`):
///
/// ```js
/// const entries = await fs.readdir(dir, { withFileTypes: true });
/// return entries
///     .filter((entry) => entry.isFile())
///     .map((entry) => entry.name)
///     .filter((name) => name.endsWith(".jsonl"))
///     .map((name) => path.join(dir, name));
/// ```
///
/// `fs.readdir` without `recursive`, and an `isFile()` guard. Nothing in the
/// shipped package creates a subdirectory under an agent's `sessions/` either:
/// the only `mkdir`s that touch it are `mkdir(sessionsDir)` and
/// `mkdir(path.dirname(sessionFile))`, both of which *are* that directory.
///
/// `is_session_path` already transcribes the file-name half exactly, so the
/// fix is the walk — `max_depth(1)` — and not a second, redundant position
/// check in the predicate.
#[test]
fn clawdbot_does_not_walk_below_its_sessions_directory() {
    let tmp = TempDir::new().expect("tempdir");
    // `CLAWDBOT_HOME` names the sessions directory itself.
    let sessions = tmp.path().join("clawdbot");

    write(&sessions.join("S-0001.jsonl"), &pi_transcript("S-0001"));
    write(
        &sessions.join("attachments").join("att.jsonl"),
        &pi_transcript("att"),
    );
    write(
        &sessions.join("archive").join("old.jsonl"),
        &pi_transcript("old"),
    );
    write(
        &sessions.join("a").join("b").join("c").join("deep.jsonl"),
        &pi_transcript("deep"),
    );

    let envelope = list_json(&tmp);
    let listed = rows(&envelope, "clawdbot", &sessions);
    let skipped = warnings(&envelope, "clawdbot", &sessions);

    assert_eq!(
        listed,
        vec!["S-0001.jsonl"],
        "ClawdBot reads one directory level; got rows {listed:?} and \
         warnings {skipped:?}"
    );
}

// ---------------------------------------------------------------------------
// OpenClaw — same shape, same fix
// ---------------------------------------------------------------------------

/// Every scanner `openclaw@2026.7.1-2` points at an agent's
/// `sessions/` directory reads exactly one level and guards on `isFile()` —
/// `doctor-state-integrity-D-B71ywJ.js:1484`,
/// `doctor-session-transcripts-CuHKQasv.js:243`,
/// `security-cli-BgOxd0Kk.js:307`, `session-write-lock-BZ_4P1vk.js:428`,
/// `store-BJJhlPrk.js:859` and `:3224`, `engine-qmd-zad3_Bbe.js:147`,
/// `cli.runtime-BQudgd-S.js:308`, `session-cost-usage-B0dBxiXW.js:226`. There
/// is no `readdir(..., { recursive: true })` anywhere in the package.
///
/// The rule is also stated outright in `paths-C2C4lJH6.js`, which refuses to
/// resolve a transcript path that is not a direct child:
///
/// ```js
/// const relativeSegments = parts.slice(sessionsIndex + 1);
/// if (relativeSegments.length !== 1) return;
/// ```
///
/// The one subdirectory OpenClaw does create there is
/// `sessions/skills-prompts/sha256/<2 hex>/<64 hex>.txt`
/// (`store-BJJhlPrk.js: readSessionPromptBlobFiles`) — a prompt blob store,
/// which is why a `.jsonl` under it is a payload and not a session.
#[test]
fn openclaw_does_not_walk_below_an_agent_sessions_directory() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = tmp
        .path()
        .join("openclaw-state")
        .join("agents")
        .join("main")
        .join("sessions");

    write(
        &sessions.join("33333333-3333-4333-8333-333333333333.jsonl"),
        &pi_transcript("33333333-3333-4333-8333-333333333333"),
    );
    write(
        &sessions
            .join("skills-prompts")
            .join("sha256")
            .join("abc123def456.jsonl"),
        &pi_transcript("abc123def456"),
    );
    write(
        &sessions.join("archive").join("old.jsonl"),
        &pi_transcript("old"),
    );
    write(
        &sessions.join("a").join("b").join("c").join("deep.jsonl"),
        &pi_transcript("deep"),
    );

    let envelope = list_json(&tmp);
    let listed = rows(&envelope, "openclaw", &sessions);
    let skipped = warnings(&envelope, "openclaw", &sessions);

    assert_eq!(
        listed,
        vec!["33333333-3333-4333-8333-333333333333.jsonl"],
        "OpenClaw reads one directory level; got rows {listed:?} and \
         warnings {skipped:?}"
    );
}

/// Resolution and listing must apply the same depth boundary.
///
/// Before this regression was fixed, both providers omitted these distinct
/// depth-three transcripts from `list` but recursively claimed the shared
/// filename in `owns_session`, turning `info deep` into a false ambiguity.
#[test]
fn claw_providers_do_not_resolve_sessions_they_would_not_list() {
    let tmp = TempDir::new().expect("tempdir");
    let clawdbot_sessions = tmp.path().join("clawdbot");
    let openclaw_sessions = tmp
        .path()
        .join("openclaw-state")
        .join("agents")
        .join("main")
        .join("sessions");

    write(
        &clawdbot_sessions
            .join("archive")
            .join("a")
            .join("deep.jsonl"),
        &pi_transcript("clawdbot-deep"),
    );
    write(
        &openclaw_sessions
            .join("archive")
            .join("b")
            .join("deep.jsonl"),
        &pi_transcript("openclaw-deep"),
    );

    let output = casr_cmd(&tmp)
        .args(["--json", "info", "deep"])
        .output()
        .expect("casr info should run");
    assert!(!output.status.success(), "an unlisted id must not resolve");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let json_start = stderr
        .find('{')
        .unwrap_or_else(|| panic!("JSON error envelope missing: {stderr}"));
    let error: serde_json::Value =
        serde_json::from_str(&stderr[json_start..]).unwrap_or_else(|e| panic!("{e}: {stderr}"));
    assert_eq!(
        error["error_type"], "SessionNotFound",
        "unlisted paths are absent, not competing ownership claims: {error}"
    );
}
