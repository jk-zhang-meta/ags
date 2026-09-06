//! Contract-level matrix tests.  These tests exercise only deterministic
//! profile/gate semantics; backend process tests live in the provider/runtime
//! harness and must supply real evidence before a launch is marked equivalent.

use std::collections::BTreeSet;

use ags::environment::{
    BackendCapabilities, BackendKind, CapabilityState, CommitmentLevel, EnvironmentProfile,
    FailureMode, GateDecision, ProbeEvidence, ProbeId,
};

const FIXTURES: &[(&str, BackendKind)] = &[
    (
        include_str!("fixtures/environment/linux-native.json"),
        BackendKind::LinuxNative,
    ),
    (
        include_str!("fixtures/environment/wsl2.json"),
        BackendKind::Wsl2,
    ),
    (
        include_str!("fixtures/environment/linux-vps.json"),
        BackendKind::LinuxVps,
    ),
    (
        include_str!("fixtures/environment/macos-native.json"),
        BackendKind::MacosNative,
    ),
];

fn profile(json: &str) -> EnvironmentProfile {
    serde_json::from_str(json).expect("fixture must deserialize")
}

fn native_capabilities(kind: BackendKind) -> BackendCapabilities {
    let mut capabilities = BackendCapabilities::new(kind);
    for probe in ProbeId::all() {
        capabilities = capabilities.record(*probe, ProbeEvidence::native());
    }
    capabilities
}

#[test]
fn every_supported_backend_fixture_validates_and_passes_complete_matrix() {
    for (json, backend) in FIXTURES {
        let profile = profile(json);
        profile.validate().expect("fixture profile must validate");
        assert_eq!(profile.backend.kind, *backend);
        assert_eq!(profile.target.os, backend.target_os());
        let report = profile.evaluate(&native_capabilities(*backend));
        assert_eq!(report.decision, GateDecision::Equivalent, "{backend:?}");
        assert_eq!(report.optional_failures, 0);
        assert_eq!(report.probes.len(), ProbeId::all().len());
    }
}

#[test]
fn each_required_probe_failure_blocks_for_each_backend() {
    for (json, backend) in FIXTURES {
        let profile = profile(json);
        for probe in ProbeId::all() {
            let capabilities =
                native_capabilities(*backend).record(*probe, ProbeEvidence::unknown());
            let report = profile.evaluate(&capabilities);
            assert_eq!(
                report.decision,
                GateDecision::Blocked,
                "{backend:?}/{probe:?}"
            );
            assert!(!report.probes[probe].passes);
        }
    }
}

#[test]
fn host_visible_carrier_and_native_conflicts_never_pass_observational_gate() {
    for (json, backend) in FIXTURES {
        let profile = profile(json);
        for probe in [
            ProbeId::WslProvenance,
            ProbeId::VpsProvenance,
            ProbeId::MacosProvenance,
            ProbeId::PlatformKernel,
            ProbeId::FilesystemProc,
            ProbeId::NetworkMetadata,
        ] {
            let capabilities =
                native_capabilities(*backend).record(probe, ProbeEvidence::host_visible());
            assert_eq!(
                profile.evaluate(&capabilities).decision,
                GateDecision::Blocked,
                "host leak {backend:?}/{probe:?}"
            );
            let capabilities =
                native_capabilities(*backend).record(probe, ProbeEvidence::mismatch());
            assert_eq!(
                profile.evaluate(&capabilities).decision,
                GateDecision::Blocked,
                "mismatch {backend:?}/{probe:?}"
            );
        }
    }
}

#[test]
fn explicit_optional_unknown_is_degraded_and_required_unknown_stays_blocked() {
    let (json, backend) = FIXTURES[0];
    let mut profile = profile(json);
    profile.policy.required.remove(&ProbeId::ResourceMemory);
    profile.policy.optional.insert(ProbeId::ResourceMemory);
    profile.policy.unimplemented = FailureMode::Degraded;

    let capabilities =
        native_capabilities(backend).record(ProbeId::ResourceMemory, ProbeEvidence::unknown());
    let report = profile.evaluate(&capabilities);
    assert_eq!(report.decision, GateDecision::Degraded);
    assert_eq!(report.optional_failures, 1);

    profile.policy.optional.remove(&ProbeId::ResourceMemory);
    profile.policy.required.insert(ProbeId::ResourceMemory);
    let report = profile.evaluate(&capabilities);
    assert_eq!(report.decision, GateDecision::Blocked);
}

#[test]
fn target_view_redacts_backend_routing_and_preserves_target_fields() {
    for (json, backend) in FIXTURES {
        let profile = profile(json);
        let view = profile.agent_view();
        let view_json = serde_json::to_string(&view).expect("view serializes");
        assert!(!view_json.contains("endpoint"));
        assert!(!view_json.contains("ssh://target.example"));
        assert_eq!(view.target.os, backend.target_os());
        assert_eq!(view.identity.hostname, profile.identity.hostname);
        assert_eq!(view.paths.cwd, profile.paths.cwd);
    }
}

#[test]
fn system_commitment_requires_native_or_brokered_evidence() {
    let (json, backend) = FIXTURES[0];
    let mut profile = profile(json);
    profile.policy.commitment = CommitmentLevel::SystemEquivalent;
    let mut capabilities = native_capabilities(backend);
    capabilities = capabilities.record(ProbeId::PlatformKernel, ProbeEvidence::virtualised());
    assert_eq!(
        profile.evaluate(&capabilities).decision,
        GateDecision::Blocked
    );
    let capabilities = capabilities.record(ProbeId::PlatformKernel, ProbeEvidence::brokered());
    assert_eq!(
        profile.evaluate(&capabilities).decision,
        GateDecision::Equivalent
    );
}

#[test]
fn policy_round_trip_preserves_probe_sets_and_rules() {
    let (json, _) = FIXTURES[0];
    let mut profile = profile(json);
    profile.policy.optional.insert(ProbeId::ResourceMemory);
    profile.policy.required.remove(&ProbeId::ResourceMemory);
    profile.policy.allowed_host_visible = BTreeSet::from([ProbeId::Tty]);
    let encoded = serde_json::to_string(&profile).expect("profile serializes");
    let decoded: EnvironmentProfile = serde_json::from_str(&encoded).expect("round trip");
    assert_eq!(decoded.policy.optional, profile.policy.optional);
    assert_eq!(
        decoded.policy.allowed_host_visible,
        profile.policy.allowed_host_visible
    );
    assert_eq!(decoded.target.image, profile.target.image);
}

#[test]
fn every_nonmatching_state_is_nonpassing() {
    let (json, backend) = FIXTURES[0];
    let profile = profile(json);
    for state in [
        CapabilityState::HostVisible,
        CapabilityState::Unknown,
        CapabilityState::Mismatch,
        CapabilityState::Error,
        CapabilityState::Conflict,
    ] {
        let evidence = ProbeEvidence {
            state,
            ..ProbeEvidence::unknown()
        };
        let report =
            profile.evaluate(&native_capabilities(backend).record(ProbeId::TimeRealtime, evidence));
        assert_eq!(report.decision, GateDecision::Blocked, "{state:?}");
    }
}
