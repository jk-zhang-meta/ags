//! Every provider must be launchable, not just printable.
//!
//! The default [`Provider::launch_spec`] recovers a structured spec by
//! splitting the provider's own `resume_command`. That only holds if every
//! provider's rendered command is actually a well-formed command line, which
//! is an assumption about seventeen independent implementations rather than a
//! guarantee. These tests check it against the real registry, so that adding a
//! provider whose resume form is prose, or is unbalanced, or is empty, fails
//! here rather than at launch time in front of a user.

mod test_env;

use casr::discovery::ProviderRegistry;
use casr::launch::{LaunchError, LaunchSpec, SessionTargeting};

/// Every test here resolves a provider's launch spec, and Pi-Agent's reads
/// `PI_AGENT_HOME`. One test writes it, so all of them serialize on the test
/// binary's global environment lock — concurrent read/write of the process
/// environment is unsound in Rust 2024, not merely flaky.
static ENV: test_env::EnvLock = test_env::EnvLock;

const SESSION: &str = "019c3eae-94c3-7d73-9b2a-9edb18f1563b";

#[test]
fn every_provider_yields_a_launchable_spec() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    let providers = registry.all_providers();
    assert!(providers.len() >= 17, "registry shrank unexpectedly");

    for provider in providers {
        let rendered = provider.resume_command(SESSION);
        let spec = provider.launch_spec(SESSION).unwrap_or_else(|| {
            panic!(
                "{}: resume_command {rendered:?} cannot be split into a program \
                 and arguments, so the session can be described but not started",
                provider.name()
            )
        });

        assert!(
            !spec.program.trim().is_empty(),
            "{}: empty program",
            provider.name()
        );
        assert!(
            !spec.program.contains(char::is_whitespace),
            "{}: program {:?} contains whitespace, which means the split put \
             more than one word in it",
            provider.name(),
            spec.program
        );
    }
}

#[test]
fn the_displayed_command_is_the_one_that_runs() {
    let _lock = ENV.lock().unwrap();
    // The whole reason for the structured form: what the user is shown must
    // parse back to exactly what will be executed. A provider that quotes
    // badly would show one command and run another.
    let registry = ProviderRegistry::default_registry();
    for provider in registry.all_providers() {
        let spec = provider
            .launch_spec(SESSION)
            .unwrap_or_else(|| panic!("{}: no spec", provider.name()));
        let reparsed = LaunchSpec::from_command_line(&spec.display())
            .unwrap_or_else(|| panic!("{}: display form does not re-parse", provider.name()));
        assert_eq!(
            (reparsed.program, reparsed.args),
            (spec.program.clone(), spec.args.clone()),
            "{}: display and execution disagree",
            provider.name()
        );
    }
}

#[test]
fn session_targeting_is_reported_honestly() {
    let _lock = ENV.lock().unwrap();
    // A spec that has lost the session id would cheerfully start the agent on
    // the wrong conversation, or on none. That is unavoidable for a few
    // providers, so the requirement is not "always targeted" but "never
    // claims to be targeted when it is not".
    let registry = ProviderRegistry::default_registry();
    let (mut targeted, mut untargeted) = (Vec::new(), Vec::new());

    for provider in registry.all_providers() {
        let spec = provider
            .launch_spec(SESSION)
            .unwrap_or_else(|| panic!("{}: no spec", provider.name()));
        let carries_id = spec.args.iter().any(|arg| arg.contains(SESSION));
        assert_eq!(
            spec.targeting() == SessionTargeting::ById,
            carries_id,
            "{}: targeting says {:?} but args are {:?}",
            provider.name(),
            spec.targeting(),
            spec.args
        );
        if carries_id {
            targeted.push(provider.name().to_string());
        } else {
            untargeted.push(provider.name().to_string());
        }
    }

    // Pinned so that a provider silently losing its session id shows up here
    // as a diff rather than as a user launching into the wrong conversation.
    untargeted.sort();
    assert_eq!(
        untargeted,
        ["Aider", "Cline", "Cursor"],
        "the set of providers that cannot be launched at a specific session changed"
    );
    assert!(targeted.len() >= 14);
}

#[test]
fn passthrough_cannot_retarget_a_converted_session() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    let claude = registry
        .find_by_slug("claude-code")
        .expect("claude-code in registry");
    let spec = claude.launch_spec(SESSION).expect("spec");

    let ok = spec
        .clone()
        .try_passthrough(["--model", "opus"])
        .expect("an unrelated flag is fine");
    assert!(ok.display().ends_with("--model opus"));

    let err = spec
        .try_passthrough(["--resume", "a-different-session"])
        .expect_err("re-specifying --resume must be refused");
    assert_eq!(
        err,
        LaunchError::ConflictingFlag {
            flag: "--resume".into()
        }
    );
}

/// A value that needs shell quoting must never produce a *false* targeting
/// claim.
///
/// The registry's default `launch_spec` recovers argv by splitting the
/// provider's rendered `resume_command`, so a provider that interpolates any
/// value containing whitespace hands the agent a different argument vector than
/// the one it meant. The split cannot detect that — the broken line re-renders
/// to itself — so the requirement is the weaker, checkable one: whatever argv
/// comes out, a spec that says it targets the session must actually carry the
/// session id in one argument.
#[test]
fn a_value_that_needs_quoting_never_yields_a_false_targeting_claim() {
    let _lock = ENV.lock().unwrap();
    const AWKWARD: &str = "id with space";
    let registry = ProviderRegistry::default_registry();
    for provider in registry.all_providers() {
        let Some(spec) = provider.launch_spec(AWKWARD) else {
            continue;
        };
        if spec.targeting() == SessionTargeting::ById {
            assert!(
                spec.args.iter().any(|arg| arg.contains(AWKWARD)),
                "{}: claims to target the session but no single argument carries it: {:?}",
                provider.name(),
                spec.args
            );
        }
    }
}

/// Providers whose native resume forms interpolate the id must build argv
/// directly. This pins the exact vendor commands and proves a space remains
/// part of one argument rather than becoming an extra word after rendering.
#[test]
fn corrected_resume_commands_keep_the_session_id_as_one_argument() {
    let _lock = ENV.lock().unwrap();
    const AWKWARD: &str = "id with space";
    let registry = ProviderRegistry::default_registry();
    let cases = [
        ("factory", "droid", vec!["--resume", AWKWARD]),
        (
            "clawdbot",
            "clawdbot",
            vec!["tui", "--session", "agent:main:id with space"],
        ),
        (
            "openclaw",
            "openclaw",
            vec!["tui", "--session", "agent:main:id with space"],
        ),
        ("opencode", "opencode", vec!["--session", AWKWARD]),
    ];

    for (slug, program, args) in cases {
        let provider = registry
            .find_by_slug(slug)
            .unwrap_or_else(|| panic!("{slug}: provider missing"));
        let spec = provider
            .launch_spec(AWKWARD)
            .unwrap_or_else(|| panic!("{slug}: no launch spec"));
        assert_eq!(spec.program, program, "{slug}: wrong executable");
        assert_eq!(
            spec.args,
            args.into_iter().map(str::to_string).collect::<Vec<_>>(),
            "{slug}: wrong argv"
        );
        assert_eq!(
            spec.targeting(),
            SessionTargeting::ById,
            "{slug}: corrected command must target the written session"
        );
        let reparsed = LaunchSpec::from_command_line(&spec.display()).expect("display re-parses");
        assert_eq!(reparsed.args, spec.args, "{slug}: display changed argv");
    }

    let clawdbot = registry.find_by_slug("clawdbot").unwrap();
    let spec = clawdbot
        .launch_spec("Native-ID")
        .expect("existing native ClawdBot sessions remain resumable");
    assert_eq!(
        spec.args,
        ["tui", "--session", "agent:main:native-id"].map(str::to_string),
        "ClawdBot lowercases the full native session key before TUI lookup"
    );

    let openclaw = registry.find_by_slug("openclaw").unwrap();
    let spec = openclaw
        .launch_spec("Native-ID")
        .expect("existing native OpenClaw sessions remain resumable");
    assert_eq!(
        spec.args,
        ["tui", "--session", "agent:main:native-id"].map(str::to_string),
        "OpenClaw lowercases the full native session key before TUI lookup"
    );
}

/// An empty session id targets nothing.
///
/// Every string contains the empty string, so the substring rule on its own
/// reported all seventeen providers as pointed at a session that does not exist.
#[test]
fn an_empty_session_id_targets_nothing() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    for provider in registry.all_providers() {
        let Some(spec) = provider.launch_spec("") else {
            continue;
        };
        assert_eq!(
            spec.targeting(),
            SessionTargeting::NotTargeted,
            "{}: an absent id cannot be a targeted one",
            provider.name()
        );
    }
}

/// Pi-Agent's path argument survives a home directory with a space in it.
///
/// The only provider whose resume form interpolates a filesystem path. Rendered
/// to a string and split back apart, `PI_AGENT_HOME=/tmp/Pi Home` produced
/// `["--session", "/tmp/Pi", "Home/sessions/<id>.jsonl"]`: `pi` opened
/// `/tmp/Pi`, and the stray third word still contained the id, so the launcher
/// reported the session as targeted. The spec is built from the path now rather
/// than recovered from a rendering of it.
///
/// `/tmp/Pi Home` holds no session, which is the point of using it here: an id
/// that names no file on disk is answered with the flat `sessions/<id>.jsonl`
/// location, the only one an id alone can name. Where the session *does* exist
/// the answer is the file itself — `pi_is_launched_at_the_file_it_wrote`.
#[test]
fn pi_path_argument_survives_a_home_with_a_space() {
    let _lock = ENV.lock().unwrap();
    let _guard = EnvGuard::set("PI_AGENT_HOME", "/tmp/Pi Home");
    let registry = ProviderRegistry::default_registry();
    let pi = registry
        .find_by_slug("pi-agent")
        .expect("pi-agent in registry");
    let spec = pi.launch_spec(SESSION).expect("spec");

    assert_eq!(spec.program, "pi");
    assert_eq!(
        spec.args,
        vec![
            "--session".to_string(),
            format!("/tmp/Pi Home/sessions/{SESSION}.jsonl"),
        ],
        "the whole path has to arrive as one argument"
    );
    assert_eq!(spec.targeting(), SessionTargeting::ById);
    // And the displayed form still parses back to what runs.
    let reparsed = LaunchSpec::from_command_line(&spec.display()).expect("display re-parses");
    assert_eq!(reparsed.args, spec.args);
}

/// Pi-Agent is launched at the file it wrote, wherever the workspace put it.
///
/// `pi` offers a session in `pi --resume` and `/sessions` only out of the two
/// directories its listers read — `<agent-dir>/sessions/<dir>/*.jsonl` for
/// `listAll` and `getDefaultSessionDir(cwd)` flat for `list` — so the writer
/// files a converted session under the encoded workspace name rather than
/// beside `sessions/`. That makes the location a function of the session's
/// workspace and not of its id, which is why the launch spec finds the file
/// instead of computing its path. Measured against
/// `@mariozechner/pi-coding-agent@0.73.1`: both listers return this file, and
/// neither returns the same session at `sessions/<id>.jsonl`.
#[test]
fn pi_is_launched_at_the_file_it_wrote() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::TempDir::new().expect("tempdir");
    let _guard = EnvGuard::set("PI_AGENT_HOME", &home.path().display().to_string());
    let registry = ProviderRegistry::default_registry();
    let pi = registry
        .find_by_slug("pi-agent")
        .expect("pi-agent in registry");

    let session = casr::model::CanonicalSession {
        session_id: format!("2026-01-01T00-00-00_{SESSION}"),
        provider_slug: "test-source".to_string(),
        workspace: Some(std::path::PathBuf::from("/data/projects/myapp")),
        title: None,
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000),
        messages: vec![casr::model::CanonicalMessage {
            idx: 0,
            role: casr::model::MessageRole::User,
            content: "hello".to_string(),
            timestamp: Some(1_700_000_000_000),
            author: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            extra: serde_json::Value::Null,
        }],
        model_name: None,
        metadata: serde_json::Value::Null,
        source_path: std::path::PathBuf::from("/dev/null"),
    };

    let written = pi
        .write_session(&session, &casr::providers::WriteOptions { force: false })
        .expect("write_session");

    let expected = home
        .path()
        .join("sessions")
        .join("--data-projects-myapp--")
        .join(format!("2026-01-01T00-00-00_{SESSION}.jsonl"));
    assert_eq!(
        written.paths[0], expected,
        "a converted session belongs in the directory `pi`'s own listers read"
    );

    let spec = pi.launch_spec(&written.session_id).expect("spec");
    assert_eq!(
        spec.args,
        vec!["--session".to_string(), expected.display().to_string(),],
        "the launch spec must name the file that was written, not where an id \
         alone would have put it"
    );
}

/// A session written before the layout changed still resumes at its own path.
///
/// Round 7 admitted depth 1 under `sessions/` on purpose, so that nothing
/// already on disk stopped being listable. The resume command has to hold the
/// same line: `pi --session <path>` takes its argument verbatim
/// (`dist/main.js:106-109`), so a session left at `sessions/<id>.jsonl` opens
/// from there — but only if the spec still names *that* path rather than the
/// encoded-workspace one a fresh write would use.
#[test]
fn pi_still_launches_a_session_left_at_the_old_flat_path() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::TempDir::new().expect("tempdir");
    let _guard = EnvGuard::set("PI_AGENT_HOME", &home.path().display().to_string());
    let registry = ProviderRegistry::default_registry();
    let pi = registry
        .find_by_slug("pi-agent")
        .expect("pi-agent in registry");

    let flat = home.path().join("sessions");
    std::fs::create_dir_all(&flat).expect("sessions dir");
    let legacy = flat.join(format!("{SESSION}.jsonl"));
    std::fs::write(
        &legacy,
        "{\"type\":\"session\",\"id\":\"x\",\"cwd\":\"/data/projects/myapp\"}\n",
    )
    .expect("legacy session");

    let spec = pi.launch_spec(SESSION).expect("spec");
    assert_eq!(
        spec.args,
        vec!["--session".to_string(), legacy.display().to_string()],
    );
}

struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}
