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
        ["Aider", "Cline", "Cursor", "OpenCode"],
        "the set of providers that cannot be launched at a specific session changed"
    );
    assert!(targeted.len() >= 13);
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

/// Pi-Agent is launched at the file it wrote, whatever its home is called.
///
/// The only provider whose resume form interpolates a filesystem path. Rendered
/// to a string and split back apart, `PI_AGENT_HOME=/tmp/Pi Home` produced
/// `["--session", "/tmp/Pi", "Home/sessions/<id>.jsonl"]`: `pi` opened
/// `/tmp/Pi`, and the stray third word still contained the id, so the launcher
/// reported the session as targeted. The spec is built from the path now rather
/// than recovered from a rendering of it.
#[test]
fn pi_is_launched_at_the_session_path_it_wrote() {
    let _lock = ENV.lock().unwrap();
    let _guard = EnvGuard::set("PI_AGENT_HOME", "/tmp/Pi Home");
    let registry = ProviderRegistry::default_registry();
    let pi = registry.find_by_slug("pi-agent").expect("pi-agent in registry");
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
