//! Every provider must be launchable, not just printable.
//!
//! The default [`Provider::launch_spec`] recovers a structured spec by
//! splitting the provider's own `resume_command`. That only holds if every
//! provider's rendered command is actually a well-formed command line, which
//! is an assumption about seventeen independent implementations rather than a
//! guarantee. These tests check it against the real registry, so that adding a
//! provider whose resume form is prose, or is unbalanced, or is empty, fails
//! here rather than at launch time in front of a user.

use casr::discovery::ProviderRegistry;
use casr::launch::{LaunchError, LaunchSpec, SessionTargeting};

const SESSION: &str = "019c3eae-94c3-7d73-9b2a-9edb18f1563b";

#[test]
fn every_provider_yields_a_launchable_spec() {
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
