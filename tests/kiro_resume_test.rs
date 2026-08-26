//! What `casr resume kr <id>` prints, and what the Kiro IDE reader keeps.
//!
//! Two defects with one cause: casr treated a Kiro session id as a single
//! thing. It is not. `kiro-cli --resume-id <id>` is implemented by handing the
//! id straight to the internal wire subcommand `chat _ ensure-session`, and
//! from the shipped TUI bundle that call is:
//!
//! ```js
//! let Q = await md({ sourceFormat: "auto", sourceSessionId: zn.resumeId,
//!                    targetFormat: Lc(), cwd: process.cwd() });
//! ```
//!
//! so the resolution is scoped by the working directory and by the agent
//! engine. Running that subcommand against `kiro-cli-chat` 2.14.2 directly,
//! in a sandboxed `HOME`, with no network and no login — one bucketed session
//! whose workspace is `…/ws-demo`:
//!
//! ```text
//! --cwd …/ws-demo --target-format kas → {"kind":"ensureSession","data":{"sessionId":"sess_9c1f…"}}
//! --cwd …/ws-imp  --target-format kas → {"kind":"error","data":{…,"code":"SESSION_NOT_FOUND"}}
//! --cwd …/ws-demo --target-format v2  → {"kind":"error","data":{"message":"ensure-session: KAS source -> V2 target not supported"}}
//! ```
//!
//! and one flat `sessions/cli/<uuid>.json` session, which resolves from
//! `--cwd /tmp` and from an unrelated workspace alike. `--target-format`
//! follows the engine and the engine defaults to v2, so a bucketed session
//! needs `--v3` as well as the workspace.
//!
//! The Kiro IDE adds nothing: it has no CLI-invocable resume for a session
//! already on disk. Its only deep link is
//! `kiro://kiro.resume-session/<base64 presigned-URL>`, whose handler
//! downloads and unpacks a *remote* zip into a folder the user picks
//! (`src/extension/session-resume/resume-session-uri-handler.ts` in the
//! shipped extension.js), and its only local affordance is the palette
//! command `kiroAgent.openChatSession`.

mod test_env;

use std::path::{Path, PathBuf};

use ags::launch::SessionTargeting;
use ags::providers::Provider;
use ags::providers::kiro::Kiro;

static ENV: test_env::EnvLock = test_env::EnvLock;

const WORKSPACE: &str = "/home/u/demo-project";
const BUCKET: &str = "b550143462be8201";
const IDE_ID: &str = "sess_9c1f4c2e-6b0a-4f71-8a55-2f0d7b3ac914";
const FLAT_ID: &str = "0a5376f2-7e2f-4981-bcbc-67195586604a";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: the caller holds `ENV` for the duration, so no other thread
        // reads or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }

    fn remove(key: &'static str) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// `<root>/sessions/<bucket>/<id>/{session.json,messages.jsonl}`, optionally
/// with the workspace rewritten or emptied.
fn seed_bucketed(root: &Path, bucket: &str, id: &str, workspaces: &[&str]) -> PathBuf {
    let dir = root.join("sessions").join(bucket).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("kiro/ide_session.json")).unwrap(),
    )
    .unwrap();
    meta["id"] = serde_json::Value::String(id.to_string());
    meta["workspacePaths"] = serde_json::Value::Array(
        workspaces
            .iter()
            .map(|w| serde_json::Value::String((*w).to_string()))
            .collect(),
    );
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    std::fs::copy(
        fixtures_dir().join("kiro/ide_messages.jsonl"),
        dir.join("messages.jsonl"),
    )
    .unwrap();
    dir.join("session.json")
}

/// The flat triplet at `<root>/sessions/cli/<uuid>.{json,jsonl,history}`.
fn seed_flat(root: &Path) {
    let cli = root.join("sessions").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    for ext in ["json", "jsonl", "history"] {
        std::fs::copy(
            fixtures_dir().join(format!("kiro/{FLAT_ID}.{ext}")),
            cli.join(format!("{FLAT_ID}.{ext}")),
        )
        .unwrap();
    }
}

/// A flat session resolves from anywhere, so the command carries no directory.
#[test]
fn flat_session_resumes_with_the_bare_flag() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::remove("KIRO_HOME");
    seed_flat(&home.path().join(".kiro"));

    assert_eq!(
        Kiro.resume_command(FLAT_ID),
        format!("kiro-cli --resume-id {FLAT_ID}")
    );
    let spec = Kiro.launch_spec(FLAT_ID).expect("spec");
    assert_eq!(spec.program, "kiro-cli");
    assert!(spec.cwd.is_none(), "a flat session is not workspace-scoped");
    assert_eq!(spec.targeting(), SessionTargeting::ById);
}

/// A bucketed session is only findable from its own workspace, and only by the
/// KAS engine. Before the fix casr printed the bare flag for this too, which
/// resolves nothing from anywhere else and nothing at all under the default
/// engine.
#[test]
fn bucketed_session_resumes_from_its_workspace_under_v3() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::remove("KIRO_HOME");
    seed_bucketed(&home.path().join(".kiro"), BUCKET, IDE_ID, &[WORKSPACE]);

    assert_eq!(
        Kiro.resume_command(IDE_ID),
        format!("cd {WORKSPACE} && kiro-cli --v3 --resume-id {IDE_ID}"),
        "the working directory is part of the lookup key, so it is part of the \
         command; --v3 because --target-format follows the engine and the \
         default engine cannot open a KAS session at all"
    );

    let spec = Kiro.launch_spec(IDE_ID).expect("spec");
    assert_eq!(spec.program, "kiro-cli", "`cd` is not the program");
    assert_eq!(spec.args, ["--v3", "--resume-id", IDE_ID]);
    assert_eq!(
        spec.cwd.as_deref(),
        Some(Path::new(WORKSPACE)),
        "the directory is a field of the spec, not a word in a string"
    );
    assert_eq!(spec.targeting(), SessionTargeting::ById);
}

/// A workspace path with a space in it is ordinary, and `cd /home/u/my
/// project` is not a `cd` into `/home/u/my project`.
#[test]
fn a_workspace_with_a_space_is_quoted() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::remove("KIRO_HOME");
    let spaced = "/home/u/my project";
    seed_bucketed(&home.path().join(".kiro"), BUCKET, IDE_ID, &[spaced]);

    let rendered = Kiro.resume_command(IDE_ID);
    assert!(
        rendered.starts_with("cd '/home/u/my project' &&")
            || rendered.starts_with("cd \"/home/u/my project\" &&"),
        "the directory must survive being pasted into a shell: {rendered}"
    );
    assert_eq!(
        Kiro.launch_spec(IDE_ID).expect("spec").cwd.as_deref(),
        Some(Path::new(spaced))
    );
}

/// `workspacePaths: []` is Kiro's `_global` bucket. No `process.cwd()` hashes
/// to `_global`, so `--resume-id` can never find one of these, and the IDE has
/// no per-session CLI resume to fall back on. casr says that by naming no
/// session rather than by printing a command that resolves nothing.
#[test]
fn a_global_bucketed_session_is_reported_as_unreachable() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::remove("KIRO_HOME");
    let global_id = "sess_3d7e5a10-11c2-4c8e-9b64-5a1e0f2d8c37";
    seed_bucketed(&home.path().join(".kiro"), "_global", global_id, &[]);

    assert_eq!(
        Kiro.resume_command(global_id),
        "kiro",
        "there is no command that resumes this session; opening the IDE is the \
         whole truth and the resume line must not imply more"
    );
    let spec = Kiro.launch_spec(global_id).expect("spec");
    assert_eq!(spec.targeting(), SessionTargeting::NotTargeted);
    assert!(
        !spec.args.iter().any(|a| a.contains(global_id)),
        "claiming to target a session it cannot reach is the defect, not the fix"
    );
}

/// `KIRO_HOME` relocates kiro-cli's bucketed sessions too, so the workspace
/// lookup has to look there as well — otherwise the resume line silently
/// degrades to the flat form for a session that is not flat.
#[test]
fn a_bucketed_session_under_kiro_home_still_resolves_its_workspace() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::set("KIRO_HOME", kiro_home.path());
    let cli_kas_id = "cli_0a5376f2-7e2f-4981-bcbc-67195586604a_uHORXqEL";
    seed_bucketed(kiro_home.path(), BUCKET, cli_kas_id, &[WORKSPACE]);

    assert_eq!(
        Kiro.resume_command(cli_kas_id),
        format!("cd {WORKSPACE} && kiro-cli --v3 --resume-id {cli_kas_id}")
    );
}

/// `session_start` is the opening user turn, not a lifecycle marker.
///
/// In the shipped extension.js it is built from the session's first prompt and
/// written once, on the first turn only, with no `user` payload beside it
/// (`Se17` → `V19`); Kiro's own model-context rebuild replays it as a human
/// message ahead of everything else (`ke19` →
/// `pt3.fromHuman(messageId).withText(content)`). Dropping it dropped the
/// first thing the user said, in every IDE session casr read.
#[test]
fn session_start_is_the_opening_user_message() {
    let tmp = tempfile::tempdir().unwrap();
    let anchor = seed_bucketed(tmp.path(), BUCKET, IDE_ID, &[WORKSPACE]);

    let session = Kiro.read_session(&anchor).expect("session parses");
    let first = session.messages.first().expect("at least one message");
    assert_eq!(
        first.content, "Add a /health endpoint to the server.",
        "session_start.content is the prompt that opened the session"
    );
    assert_eq!(first.role, ags::model::MessageRole::User);

    // And it is not double-counted against the mid-turn `user` payload, which
    // is a different prompt.
    let user_texts: Vec<&str> = session
        .messages
        .iter()
        .filter(|m| m.role == ags::model::MessageRole::User)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        user_texts,
        [
            "Add a /health endpoint to the server.",
            "Use a plain route, not a blueprint."
        ]
    );

    // The lifecycle payloads that really are lifecycle payloads stay out.
    assert!(
        !session
            .messages
            .iter()
            .any(|m| m.content.contains("end_turn") || m.content.contains("promptTurnSummaries")),
        "turn_start / turn_end / usage_summary are not messages"
    );
}
