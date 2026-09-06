//! Typed contract for an agent-visible operating-system environment.
//!
//! This module deliberately does not try to virtualise the operating system.
//! It is the small, dependency-free seam between a launcher and a future
//! runtime adapter: a profile describes the target view, a backend describes
//! where work is executed, and a capability report decides whether the
//! requested view is safe to advertise.
//!
//! Backend provenance is kept separate from the target platform.  In
//! particular, WSL and a VPS are Linux execution backends, but their carrier
//! identity must never be copied into an agent-facing profile.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Version of the serialised environment contract.
pub const ENVIRONMENT_CONTRACT_V1: &str = "environment-contract-v1";

/// Operating system the agent is meant to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetOs {
    Linux,
    Macos,
}

/// Where the process is actually executed.  This is audit data, not target
/// identity; callers must not render it into model-visible environment data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    LinuxNative,
    Wsl2,
    LinuxVps,
    MacosNative,
}

impl BackendKind {
    /// The OS semantics supplied by this backend.
    pub const fn target_os(self) -> TargetOs {
        match self {
            Self::LinuxNative | Self::Wsl2 | Self::LinuxVps => TargetOs::Linux,
            Self::MacosNative => TargetOs::Macos,
        }
    }

    /// Stable carrier label for audit and routing.  It must not be placed in
    /// an agent-facing profile unless the user explicitly asks for it.
    pub const fn carrier(self) -> &'static str {
        match self {
            Self::LinuxNative => "native",
            Self::Wsl2 => "wsl2",
            Self::LinuxVps => "vps",
            Self::MacosNative => "native-macos",
        }
    }
}

/// Backend routing information.  `endpoint` is intentionally optional so a
/// local WSL/native backend and a remote VPS/Mac can share the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSpec {
    pub kind: BackendKind,
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl BackendSpec {
    pub const fn local(kind: BackendKind) -> Self {
        Self {
            kind,
            endpoint: None,
        }
    }

    pub fn remote(kind: BackendKind, endpoint: impl Into<String>) -> Self {
        Self {
            kind,
            endpoint: Some(endpoint.into()),
        }
    }
}

/// Target OS identity shown to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlatform {
    pub os: TargetOs,
    #[serde(default)]
    pub distribution: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    /// Expected kernel release/build contract.  This is optional for legacy
    /// profiles, but a profile that wants kernel observational equivalence
    /// should pin it (or route to a backend that natively supplies it).
    #[serde(default)]
    pub kernel: Option<KernelContract>,
    /// Userspace ABI contract (for example `linux-gnu` or `darwin`).
    #[serde(default)]
    pub abi: Option<String>,
    /// Immutable target image/rootfs reference.  A digest or remote target
    /// reference lets a report prove which real environment supplied facts.
    #[serde(default)]
    pub image: Option<ArtifactReference>,
}

/// Kernel and ABI facts that may be observed directly by native programs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelContract {
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub abi: Option<String>,
}

/// A pinned immutable artifact or a remote target identity.  Hashes are
/// deliberately strings so callers can use a registry digest (`sha256:...`)
/// or a plain hexadecimal SHA-256 without changing the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub reference: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Target user identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityProfile {
    pub hostname: String,
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub umask: Option<u32>,
    /// Capability names exposed by `/proc/self/status`, `capsh`, or an
    /// equivalent broker.  Empty means the profile does not pin capabilities.
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    #[serde(default)]
    pub passwd: Option<ArtifactReference>,
}

/// Wall-clock contract. `anchor_epoch_seconds` is the virtual UTC instant;
/// `rate` describes how virtual wall time advances relative to real time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockProfile {
    pub timezone: String,
    pub tzdata: String,
    pub anchor_epoch_seconds: i64,
    #[serde(default = "default_clock_rate")]
    pub rate: f64,
    /// Optional monotonic/boottime anchors.  They are separate from realtime
    /// because `Instant` and `clock_gettime` do not use the wall clock.
    #[serde(default)]
    pub monotonic_anchor_ns: Option<i128>,
    #[serde(default)]
    pub boottime_anchor_ns: Option<i128>,
    /// Hash of the exact tzdata/ICU timezone artifact.  `tzdata: backend` is
    /// rejected unless the profile explicitly accepts backend differences.
    #[serde(default)]
    pub tzdata_sha256: Option<String>,
    #[serde(default)]
    pub allow_backend_tzdata: bool,
    /// Explicit UTC offset for virtual `date` when `timezone` is an IANA name.
    /// Launch overlay still sets `TZ` to the IANA name so libc/ICU see a zone.
    #[serde(default)]
    pub offset_seconds: Option<i64>,
    /// When true, observation hooks use the current real instant in the
    /// profile timezone instead of the frozen `anchor_epoch_seconds`.
    #[serde(default)]
    pub follow_realtime: bool,
}

fn default_clock_rate() -> f64 {
    1.0
}

/// Paths visible to the agent.  Runtime adapters may add mappings later; the
/// initial contract keeps the three paths that every launcher must establish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathProfile {
    pub cwd: String,
    pub home: String,
    pub tmp: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub proc: Option<String>,
    #[serde(default)]
    pub sys: Option<String>,
    #[serde(default)]
    pub mounts: Option<String>,
    #[serde(default)]
    pub self_exe: Option<String>,
    /// Host path fragments that must never appear in agent-visible output.
    #[serde(default)]
    pub hidden_host_markers: Vec<String>,
}

/// Locale contract.  ICU/glibc data should be pinned by the runtime adapter;
/// this type carries the selected locale without assuming a host database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleProfile {
    pub lang: String,
    pub lc_all: String,
    #[serde(default)]
    pub lc_time: Option<String>,
    #[serde(default)]
    pub charset: Option<String>,
    #[serde(default)]
    pub icu_version: Option<String>,
    #[serde(default)]
    pub artifact: Option<ArtifactReference>,
}

/// Network facts visible to an agent.  The backend may use a different DNS,
/// route, proxy, or address, but those values must be either translated by a
/// broker or represented as an explicit failed/host-visible capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetworkProfile {
    #[serde(default)]
    pub dns_servers: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub ca_bundle: Option<ArtifactReference>,
    /// Whether link-local metadata services (for example 169.254.169.254)
    /// are expected to be unreachable from the target view.
    #[serde(default = "default_true")]
    pub block_metadata: bool,
    #[serde(default)]
    pub hostname_resolution: Option<String>,
}

/// PTY and terminal contract.  Window size and termios are included because
/// agents commonly use `isatty`, `stty`, and SIGWINCH as environment probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtyProfile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub columns: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_true")]
    pub signals: bool,
}

impl Default for TtyProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            term: None,
            columns: None,
            rows: None,
            kind: None,
            signals: true,
        }
    }
}

/// Process and resource facts that can otherwise reveal a container, VM, or
/// host carrier.  Values are optional because a real backend may intentionally
/// leave PID allocation and cgroup layout unconstrained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcessProfile {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub ppid: Option<u32>,
    #[serde(default)]
    pub init: Option<String>,
    #[serde(default)]
    pub cgroup: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub cpu_count: Option<u32>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub open_files_limit: Option<u64>,
}

/// Resource and mount constraints are kept separate from paths so a runtime
/// can report an inability to emulate them without pretending they match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourceProfile {
    #[serde(default)]
    pub cpu_count: Option<u32>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub disk_bytes: Option<u64>,
    #[serde(default)]
    pub limits: BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

/// Probe dimensions used by the first contract.  New dimensions can be added
/// without changing the meaning of existing reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeId {
    PlatformOs,
    PlatformDistribution,
    PlatformArchitecture,
    PlatformKernel,
    PlatformAbi,
    WslProvenance,
    VpsProvenance,
    MacosProvenance,
    IdentityUid,
    IdentityGid,
    IdentityGroups,
    IdentityHostname,
    IdentityPasswd,
    IdentityCapabilities,
    TimeRealtime,
    TimeMonotonic,
    TimeMonotonicRaw,
    TimeBoottime,
    TimeCpu,
    TimeSleep,
    Timezone,
    Tzdata,
    FilesystemCwd,
    FilesystemHome,
    FilesystemTmp,
    FilesystemWorkspace,
    FilesystemProc,
    FilesystemSys,
    FilesystemMounts,
    FilesystemSelfExe,
    FilesystemStatvfs,
    Locale,
    LocaleIcu,
    Tty,
    TtyWindow,
    TtyTermios,
    TtySignals,
    NetworkDns,
    NetworkIp,
    NetworkRoute,
    NetworkProxy,
    NetworkTls,
    NetworkMetadata,
    ProcessIdentity,
    ProcessInheritance,
    ProcessForkExec,
    McpPluginInheritance,
    ResourceLimits,
    ResourceCpu,
    ResourceMemory,
}

impl ProbeId {
    /// Every probe known by this contract, in stable order for reports and
    /// matrix tests.  Keep additions at the end of their conceptual group so
    /// serialized snapshots remain easy to diff.
    pub const ALL: &'static [Self] = &[
        Self::PlatformOs,
        Self::PlatformDistribution,
        Self::PlatformArchitecture,
        Self::PlatformKernel,
        Self::PlatformAbi,
        Self::WslProvenance,
        Self::VpsProvenance,
        Self::MacosProvenance,
        Self::IdentityUid,
        Self::IdentityGid,
        Self::IdentityGroups,
        Self::IdentityHostname,
        Self::IdentityPasswd,
        Self::IdentityCapabilities,
        Self::TimeRealtime,
        Self::TimeMonotonic,
        Self::TimeMonotonicRaw,
        Self::TimeBoottime,
        Self::TimeCpu,
        Self::TimeSleep,
        Self::Timezone,
        Self::Tzdata,
        Self::FilesystemCwd,
        Self::FilesystemHome,
        Self::FilesystemTmp,
        Self::FilesystemWorkspace,
        Self::FilesystemProc,
        Self::FilesystemSys,
        Self::FilesystemMounts,
        Self::FilesystemSelfExe,
        Self::FilesystemStatvfs,
        Self::Locale,
        Self::LocaleIcu,
        Self::Tty,
        Self::TtyWindow,
        Self::TtyTermios,
        Self::TtySignals,
        Self::NetworkDns,
        Self::NetworkIp,
        Self::NetworkRoute,
        Self::NetworkProxy,
        Self::NetworkTls,
        Self::NetworkMetadata,
        Self::ProcessIdentity,
        Self::ProcessInheritance,
        Self::ProcessForkExec,
        Self::McpPluginInheritance,
        Self::ResourceLimits,
        Self::ResourceCpu,
        Self::ResourceMemory,
    ];

    pub const fn all() -> &'static [Self] {
        Self::ALL
    }

    /// Probes that can reveal where a Linux process is carried (WSL/VPS) or
    /// whether a macOS process is remote.  Their expected result is usually a
    /// redacted/virtualized absence, and they must never be copied into the
    /// agent-facing target view.
    pub const fn is_carrier_sensitive(self) -> bool {
        matches!(
            self,
            Self::WslProvenance | Self::VpsProvenance | Self::MacosProvenance
        )
    }

    /// Conservative default required set.  A caller may explicitly remove a
    /// probe from `ContractPolicy.required` when it has chosen commitment A
    /// (real backend identity), but the default contract is observational
    /// equivalence and therefore fails closed on every listed dimension.
    pub const fn default_required(self) -> bool {
        let _ = self;
        true
    }
}

/// Where a probe result came from and whether it satisfies the target view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// Backend returned the target value without an overlay.
    NativeMatch,
    /// Runtime adapter transformed the backend value to the target value.
    OverlayVirtual,
    /// A command/syscall broker supplied the value while the underlying
    /// process remained on the selected backend.
    Brokered,
    /// Backend value is exposed and may reveal the carrier.
    HostVisible,
    /// No reliable observation or adapter exists.
    Unknown,
    /// The backend returned a definitive value that conflicts with the
    /// profile's target contract.
    Mismatch,
    /// The adapter attempted the probe but encountered an execution/error
    /// condition.  Errors are never treated as a match.
    Error,
    /// Conflicting observations were detected.
    Conflict,
}

impl CapabilityState {
    const fn satisfies_required(self) -> bool {
        matches!(
            self,
            Self::NativeMatch | Self::OverlayVirtual | Self::Brokered
        )
    }
}

/// Evidence for one probe.  `observed` and `source` are intentionally strings
/// in v1 so implementations can record a redacted value without exposing a
/// provider-specific data model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeEvidence {
    pub state: CapabilityState,
    #[serde(default)]
    pub observed: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Mechanism that produced the evidence.  This is audit metadata and is
    /// intentionally distinct from `CapabilityState`: a native command can
    /// still mismatch, and a broker can return a target value.
    #[serde(default)]
    pub mechanism: Option<ProbeMechanism>,
    #[serde(default)]
    pub expected: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub probe_version: Option<String>,
}

/// How AGS obtained a probe result.  `Passthrough` is useful for optional
/// diagnostics only; it never satisfies a required probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMechanism {
    Native,
    Overlay,
    Broker,
    Passthrough,
    Unsupported,
}

impl ProbeEvidence {
    pub const fn native() -> Self {
        Self {
            state: CapabilityState::NativeMatch,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Native),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn virtualised() -> Self {
        Self {
            state: CapabilityState::OverlayVirtual,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Overlay),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn brokered() -> Self {
        Self {
            state: CapabilityState::Brokered,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Broker),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn host_visible() -> Self {
        Self {
            state: CapabilityState::HostVisible,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Passthrough),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn unknown() -> Self {
        Self {
            state: CapabilityState::Unknown,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Unsupported),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn conflict() -> Self {
        Self {
            state: CapabilityState::Conflict,
            observed: None,
            source: None,
            mechanism: None,
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn mismatch() -> Self {
        Self {
            state: CapabilityState::Mismatch,
            observed: None,
            source: None,
            mechanism: Some(ProbeMechanism::Native),
            expected: None,
            reason: None,
            probe_version: None,
        }
    }

    pub const fn error() -> Self {
        Self {
            state: CapabilityState::Error,
            observed: None,
            source: None,
            mechanism: None,
            expected: None,
            reason: None,
            probe_version: None,
        }
    }
}

/// Which observations are required before the profile can be advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractPolicy {
    #[serde(default = "default_required_probes")]
    pub required: BTreeSet<ProbeId>,
    #[serde(default)]
    pub allowed_host_visible: BTreeSet<ProbeId>,
    /// Optional probes are recorded for diagnostics.  A failed optional probe
    /// yields `degraded` instead of allowing a required host leak to pass.
    #[serde(default)]
    pub optional: BTreeSet<ProbeId>,
    #[serde(default)]
    pub commitment: CommitmentLevel,
    #[serde(default)]
    pub unimplemented: FailureMode,
    /// Per-probe mechanism declarations.  Missing rules use the conservative
    /// state policy and therefore cannot weaken the required gate.
    #[serde(default)]
    pub rules: BTreeMap<ProbeId, ProbeRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentLevel {
    /// Run on a real selected backend and expose its native identity.
    RealBackend,
    /// Profile-controlled observational equivalence over required probes.
    #[default]
    ObservationalEquivalence,
    /// System-level indistinguishability; only a true target kernel/runtime can
    /// satisfy this.  AGS rejects it unless all probes are native/brokered.
    SystemEquivalent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    #[default]
    FailClosed,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProbeRule {
    #[serde(default)]
    pub mechanisms: BTreeSet<ProbeMechanism>,
    #[serde(default)]
    pub allow_host_visible: bool,
    #[serde(default)]
    pub allow_unknown: bool,
}

impl Default for ContractPolicy {
    fn default() -> Self {
        Self {
            required: default_required_probes(),
            allowed_host_visible: BTreeSet::new(),
            optional: BTreeSet::new(),
            commitment: CommitmentLevel::ObservationalEquivalence,
            unimplemented: FailureMode::FailClosed,
            rules: BTreeMap::new(),
        }
    }
}

fn default_required_probes() -> BTreeSet<ProbeId> {
    ProbeId::ALL
        .iter()
        .copied()
        .filter(|probe| probe.default_required())
        .into_iter()
        .collect()
}

/// Return the immutable default probe set used by a newly constructed or
/// deserialized profile.  Exposing a copy lets runtime adapters build a
/// complete capability matrix without reaching into private implementation
/// details.
pub fn required_probe_set() -> BTreeSet<ProbeId> {
    default_required_probes()
}

/// Complete target-view contract.  The backend is deliberately present only
/// for routing; callers should serialise `target` and the view fields for an
/// agent, while retaining `backend` in AGS audit state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentProfile {
    pub contract: String,
    pub id: String,
    pub target: TargetPlatform,
    pub backend: BackendSpec,
    pub identity: IdentityProfile,
    pub clock: ClockProfile,
    pub locale: LocaleProfile,
    pub paths: PathProfile,
    #[serde(default)]
    pub network: NetworkProfile,
    #[serde(default)]
    pub tty: TtyProfile,
    #[serde(default)]
    pub process: ProcessProfile,
    #[serde(default)]
    pub resources: ResourceProfile,
    #[serde(default)]
    pub policy: ContractPolicy,
}

/// The subset safe to serialize into a provider-facing prompt/configuration.
/// Backend kind, endpoint, and carrier provenance are intentionally absent so
/// WSL/VPS/remote-Mac facts cannot leak through ordinary profile rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTargetView {
    pub contract: String,
    pub id: String,
    pub target: AgentPlatformView,
    pub identity: AgentIdentityView,
    pub clock: AgentClockView,
    pub locale: AgentLocaleView,
    pub paths: AgentPathView,
    pub network: AgentNetworkView,
    pub tty: TtyProfile,
    pub process: ProcessProfile,
    pub resources: ResourceProfile,
}

/// Provider-facing target structures intentionally omit AGS routing and
/// redaction metadata.  The full profile remains available to AGS audit code;
/// these views are the only structures intended for prompts/config files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPlatformView {
    pub os: TargetOs,
    #[serde(default)]
    pub distribution: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub kernel: Option<KernelContract>,
    #[serde(default)]
    pub abi: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentityView {
    pub hostname: String,
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub umask: Option<u32>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentClockView {
    pub timezone: String,
    pub tzdata: String,
    pub anchor_epoch_seconds: i64,
    pub rate: f64,
    pub monotonic_anchor_ns: Option<i128>,
    pub boottime_anchor_ns: Option<i128>,
    pub tzdata_sha256: Option<String>,
}

impl Serialize for AgentClockView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("AgentClockView", 7)?;
        state.serialize_field("timezone", &self.timezone)?;
        state.serialize_field("tzdata", &self.tzdata)?;
        state.serialize_field("anchor_epoch_seconds", &self.anchor_epoch_seconds)?;
        state.serialize_field("rate", &self.rate)?;
        state.serialize_field("monotonic_anchor_ns", &self.monotonic_anchor_ns)?;
        state.serialize_field("boottime_anchor_ns", &self.boottime_anchor_ns)?;
        state.serialize_field("tzdata_sha256", &self.tzdata_sha256)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentClockView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            timezone: String,
            tzdata: String,
            anchor_epoch_seconds: i64,
            #[serde(default = "default_clock_rate")]
            rate: f64,
            #[serde(default)]
            monotonic_anchor_ns: Option<i128>,
            #[serde(default)]
            boottime_anchor_ns: Option<i128>,
            #[serde(default)]
            tzdata_sha256: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            timezone: wire.timezone,
            tzdata: wire.tzdata,
            anchor_epoch_seconds: wire.anchor_epoch_seconds,
            rate: wire.rate,
            monotonic_anchor_ns: wire.monotonic_anchor_ns,
            boottime_anchor_ns: wire.boottime_anchor_ns,
            tzdata_sha256: wire.tzdata_sha256,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLocaleView {
    pub lang: String,
    pub lc_all: String,
    #[serde(default)]
    pub lc_time: Option<String>,
    #[serde(default)]
    pub charset: Option<String>,
    #[serde(default)]
    pub icu_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPathView {
    pub cwd: String,
    pub home: String,
    pub tmp: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub proc: Option<String>,
    #[serde(default)]
    pub sys: Option<String>,
    #[serde(default)]
    pub mounts: Option<String>,
    #[serde(default)]
    pub self_exe: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNetworkView {
    #[serde(default)]
    pub dns_servers: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "default_true")]
    pub block_metadata: bool,
    #[serde(default)]
    pub hostname_resolution: Option<String>,
}

impl EnvironmentProfile {
    /// Construct a profile while rejecting a target/backend OS mismatch.
    pub fn new(
        id: impl Into<String>,
        target: TargetPlatform,
        backend: BackendSpec,
        identity: IdentityProfile,
        clock: ClockProfile,
        locale: LocaleProfile,
        paths: PathProfile,
    ) -> Result<Self, ProfileError> {
        let profile = Self {
            contract: ENVIRONMENT_CONTRACT_V1.to_string(),
            id: id.into(),
            target,
            backend,
            identity,
            clock,
            locale,
            paths,
            network: NetworkProfile::default(),
            tty: TtyProfile::default(),
            process: ProcessProfile::default(),
            resources: ResourceProfile::default(),
            policy: ContractPolicy {
                required: default_required_probes(),
                allowed_host_visible: BTreeSet::new(),
                optional: BTreeSet::new(),
                commitment: CommitmentLevel::ObservationalEquivalence,
                unimplemented: FailureMode::FailClosed,
                rules: BTreeMap::new(),
            },
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Validate profiles loaded from disk as well as profiles built through
    /// [`Self::new`].  The launch gate calls this again so malformed or stale
    /// serialised configuration cannot bypass constructor checks.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.contract != ENVIRONMENT_CONTRACT_V1 {
            return Err(ProfileError::UnsupportedContract(self.contract.clone()));
        }
        if self.target.os != self.backend.kind.target_os() {
            return Err(ProfileError::OsMismatch {
                target: self.target.os,
                backend: self.backend.kind.target_os(),
            });
        }
        if self.backend.kind == BackendKind::LinuxVps
            && self
                .backend
                .endpoint
                .as_deref()
                .is_none_or(|endpoint| endpoint.trim().is_empty())
        {
            return Err(ProfileError::MissingRemoteEndpoint);
        }
        if self.clock.timezone.trim().is_empty()
            || self.clock.tzdata.trim().is_empty()
            || !self.clock.rate.is_finite()
            || self.clock.rate < 0.0
        {
            return Err(ProfileError::InvalidClock);
        }
        if self.clock.tzdata.eq_ignore_ascii_case("backend") && !self.clock.allow_backend_tzdata {
            return Err(ProfileError::UnpinnedTimezoneData);
        }
        validate_nonempty("profile id", &self.id)?;
        validate_nonempty("hostname", &self.identity.hostname)?;
        validate_nonempty("username", &self.identity.username)?;
        validate_path("cwd", &self.paths.cwd)?;
        validate_path("home", &self.paths.home)?;
        validate_path("tmp", &self.paths.tmp)?;
        for (field, value) in [
            ("workspace", self.paths.workspace.as_deref()),
            ("proc", self.paths.proc.as_deref()),
            ("sys", self.paths.sys.as_deref()),
            ("mounts", self.paths.mounts.as_deref()),
            ("self_exe", self.paths.self_exe.as_deref()),
        ] {
            if let Some(value) = value {
                validate_path(field, value)?;
            }
        }
        for marker in &self.paths.hidden_host_markers {
            validate_nonempty("hidden_host_marker", marker)?;
        }
        if let Some(shell) = &self.identity.shell {
            validate_nonempty("shell", shell)?;
        }
        validate_locale("LANG", &self.locale.lang)?;
        validate_locale("LC_ALL", &self.locale.lc_all)?;
        if let Some(lc_time) = &self.locale.lc_time {
            validate_locale("LC_TIME", lc_time)?;
        }
        if self.target.os == TargetOs::Macos
            && self
                .target
                .architecture
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ProfileError::MissingArchitecture);
        }
        if let Some(kernel) = &self.target.kernel {
            for (field, value) in [
                ("kernel.release", kernel.release.as_deref()),
                ("kernel.build", kernel.build.as_deref()),
                ("kernel.architecture", kernel.architecture.as_deref()),
                ("kernel.abi", kernel.abi.as_deref()),
            ] {
                if let Some(value) = value {
                    validate_nonempty(field, value)?;
                }
            }
        }
        if let Some(abi) = &self.target.abi {
            validate_nonempty("target.abi", abi)?;
        }
        if let Some(image) = &self.target.image {
            validate_artifact(image, "target.image")?;
        }
        if let Some(passwd) = &self.identity.passwd {
            validate_artifact(passwd, "identity.passwd")?;
        }
        if let Some(sha256) = &self.clock.tzdata_sha256 {
            validate_nonempty("tzdata_sha256", sha256)?;
        }
        if let Some(artifact) = &self.locale.artifact {
            validate_artifact(artifact, "locale.artifact")?;
        }
        if let Some(bundle) = &self.network.ca_bundle {
            validate_artifact(bundle, "network.ca_bundle")?;
        }
        for (field, values) in [
            ("network.dns_servers", &self.network.dns_servers),
            ("network.dns_search", &self.network.dns_search),
            ("network.addresses", &self.network.addresses),
            ("network.routes", &self.network.routes),
        ] {
            for value in values {
                validate_nonempty(field, value)?;
            }
        }
        if let Some(proxy) = &self.network.proxy {
            validate_nonempty("network.proxy", proxy)?;
        }
        if let Some(resolution) = &self.network.hostname_resolution {
            validate_nonempty("network.hostname_resolution", resolution)?;
        }
        if self
            .policy
            .required
            .iter()
            .any(|probe| self.policy.optional.contains(probe))
        {
            return Err(ProfileError::RequiredProbeMarkedOptional);
        }
        Ok(())
    }

    /// Return only target-view data.  Callers should use this when constructing
    /// provider env/prompt metadata; retaining `EnvironmentProfile` internally
    /// preserves backend routing and audit information without exposing it.
    pub fn agent_view(&self) -> AgentTargetView {
        AgentTargetView {
            contract: self.contract.clone(),
            id: self.id.clone(),
            target: AgentPlatformView {
                os: self.target.os,
                distribution: self.target.distribution.clone(),
                version: self.target.version.clone(),
                architecture: self.target.architecture.clone(),
                kernel: self.target.kernel.clone(),
                abi: self.target.abi.clone(),
            },
            identity: AgentIdentityView {
                hostname: self.identity.hostname.clone(),
                username: self.identity.username.clone(),
                uid: self.identity.uid,
                gid: self.identity.gid,
                groups: self.identity.groups.clone(),
                shell: self.identity.shell.clone(),
                umask: self.identity.umask,
                capabilities: self.identity.capabilities.clone(),
            },
            clock: AgentClockView {
                timezone: self.clock.timezone.clone(),
                tzdata: self.clock.tzdata.clone(),
                anchor_epoch_seconds: self.clock.anchor_epoch_seconds,
                rate: self.clock.rate,
                monotonic_anchor_ns: self.clock.monotonic_anchor_ns,
                boottime_anchor_ns: self.clock.boottime_anchor_ns,
                tzdata_sha256: self.clock.tzdata_sha256.clone(),
            },
            locale: AgentLocaleView {
                lang: self.locale.lang.clone(),
                lc_all: self.locale.lc_all.clone(),
                lc_time: self.locale.lc_time.clone(),
                charset: self.locale.charset.clone(),
                icu_version: self.locale.icu_version.clone(),
            },
            paths: AgentPathView {
                cwd: self.paths.cwd.clone(),
                home: self.paths.home.clone(),
                tmp: self.paths.tmp.clone(),
                workspace: self.paths.workspace.clone(),
                proc: self.paths.proc.clone(),
                sys: self.paths.sys.clone(),
                mounts: self.paths.mounts.clone(),
                self_exe: self.paths.self_exe.clone(),
            },
            network: AgentNetworkView {
                dns_servers: self.network.dns_servers.clone(),
                dns_search: self.network.dns_search.clone(),
                addresses: self.network.addresses.clone(),
                routes: self.network.routes.clone(),
                proxy: self.network.proxy.clone(),
                block_metadata: self.network.block_metadata,
                hostname_resolution: self.network.hostname_resolution.clone(),
            },
            tty: self.tty.clone(),
            process: self.process.clone(),
            resources: self.resources.clone(),
        }
    }

    /// Evaluate a backend's observed capabilities against this profile.
    pub fn evaluate(&self, capabilities: &BackendCapabilities) -> ContractReport {
        let mut probes = BTreeMap::new();
        let mut optional_failures = 0usize;
        for probe in &self.policy.required {
            let evidence = capabilities
                .probes
                .get(probe)
                .cloned()
                .unwrap_or_else(ProbeEvidence::unknown);
            let rule = self.policy.rules.get(probe);
            let legacy_allowed = self.policy.allowed_host_visible.contains(probe);
            let passes = evidence_satisfies(
                *probe,
                &evidence,
                rule,
                legacy_allowed,
                self.policy.commitment,
                false,
            );
            probes.insert(*probe, ProbeResult { evidence, passes });
        }

        for probe in &self.policy.optional {
            let evidence = capabilities
                .probes
                .get(probe)
                .cloned()
                .unwrap_or_else(ProbeEvidence::unknown);
            let rule = self.policy.rules.get(probe);
            let legacy_allowed = self.policy.allowed_host_visible.contains(probe);
            let passes = evidence_satisfies(
                *probe,
                &evidence,
                rule,
                legacy_allowed,
                self.policy.commitment,
                true,
            );
            if !passes {
                optional_failures += 1;
            }
            probes.insert(*probe, ProbeResult { evidence, passes });
        }

        let profile_valid = self.validate().is_ok();
        let backend_matches = capabilities.backend == self.backend.kind;
        let required_passes = self
            .policy
            .required
            .iter()
            .all(|probe| probes.get(probe).is_some_and(|result| result.passes));
        let passes = profile_valid && backend_matches && required_passes && optional_failures == 0;
        let degraded = profile_valid && backend_matches && required_passes && optional_failures > 0;
        ContractReport {
            contract: self.contract.clone(),
            profile_id: self.id.clone(),
            backend: capabilities.backend,
            profile_valid,
            backend_matches,
            probes,
            decision: if passes {
                GateDecision::Equivalent
            } else if degraded {
                GateDecision::Degraded
            } else {
                GateDecision::Blocked
            },
            optional_failures,
        }
    }
}

fn evidence_satisfies(
    probe: ProbeId,
    evidence: &ProbeEvidence,
    rule: Option<&ProbeRule>,
    legacy_allowed_host_visible: bool,
    commitment: CommitmentLevel,
    optional: bool,
) -> bool {
    if let Some(rule) = rule {
        if !rule.mechanisms.is_empty()
            && evidence
                .mechanism
                .is_none_or(|mechanism| !rule.mechanisms.contains(&mechanism))
        {
            return false;
        }
    }

    match evidence.state {
        CapabilityState::NativeMatch | CapabilityState::Brokered => true,
        CapabilityState::OverlayVirtual => commitment != CommitmentLevel::SystemEquivalent,
        CapabilityState::HostVisible => {
            let explicitly_allowed =
                legacy_allowed_host_visible || rule.is_some_and(|rule| rule.allow_host_visible);
            // A required host-visible result can only be accepted in the
            // explicit real-backend commitment.  In the default observational
            // contract, carrier-sensitive probes always fail closed.
            explicitly_allowed
                && (optional || commitment == CommitmentLevel::RealBackend)
                && !(probe.is_carrier_sensitive() && commitment != CommitmentLevel::RealBackend)
        }
        CapabilityState::Unknown => optional && rule.is_some_and(|rule| rule.allow_unknown),
        CapabilityState::Mismatch | CapabilityState::Error | CapabilityState::Conflict => false,
    }
}

/// Capability evidence supplied by a concrete runtime adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub backend: BackendKind,
    #[serde(default)]
    pub probes: BTreeMap<ProbeId, ProbeEvidence>,
}

impl BackendCapabilities {
    pub fn new(backend: BackendKind) -> Self {
        Self {
            backend,
            probes: BTreeMap::new(),
        }
    }

    pub fn record(mut self, probe: ProbeId, evidence: ProbeEvidence) -> Self {
        self.probes.insert(probe, evidence);
        self
    }
}

/// Result of the fail-closed gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Equivalent,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub evidence: ProbeEvidence,
    pub passes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractReport {
    pub contract: String,
    pub profile_id: String,
    pub backend: BackendKind,
    pub profile_valid: bool,
    pub backend_matches: bool,
    pub probes: BTreeMap<ProbeId, ProbeResult>,
    pub decision: GateDecision,
    #[serde(default)]
    pub optional_failures: usize,
}

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), ProfileError> {
    if value.trim().is_empty() || value.chars().any(|character| character.is_control()) {
        return Err(ProfileError::InvalidField(field));
    }
    Ok(())
}

fn validate_path(field: &'static str, value: &str) -> Result<(), ProfileError> {
    validate_nonempty(field, value)?;
    if !value.starts_with('/') {
        return Err(ProfileError::RelativePath(field));
    }
    Ok(())
}

fn validate_locale(field: &'static str, value: &str) -> Result<(), ProfileError> {
    validate_nonempty(field, value)?;
    if value.contains('/') || value.contains('\\') {
        return Err(ProfileError::InvalidField(field));
    }
    Ok(())
}

fn validate_artifact(
    artifact: &ArtifactReference,
    field: &'static str,
) -> Result<(), ProfileError> {
    validate_nonempty(field, &artifact.reference)?;
    if let Some(sha256) = &artifact.sha256 {
        validate_nonempty("artifact.sha256", sha256)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("unsupported environment contract `{0}`")]
    UnsupportedContract(String),
    #[error("target OS {target:?} does not match backend OS {backend:?}")]
    OsMismatch { target: TargetOs, backend: TargetOs },
    #[error("a Linux VPS backend requires a remote endpoint")]
    MissingRemoteEndpoint,
    #[error("clock timezone and tzdata must be non-empty and rate must be finite/non-negative")]
    InvalidClock,
    #[error(
        "timezone data must be pinned; use an artifact hash or explicitly allow backend tzdata"
    )]
    UnpinnedTimezoneData,
    #[error("profile field `{0}` is empty or contains a control character")]
    InvalidField(&'static str),
    #[error("profile path `{0}` must be absolute")]
    RelativePath(&'static str),
    #[error("macOS target profiles must declare an architecture")]
    MissingArchitecture,
    #[error("a probe cannot be both required and optional")]
    RequiredProbeMarkedOptional,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(backend: BackendKind) -> EnvironmentProfile {
        EnvironmentProfile::new(
            "test-linux",
            TargetPlatform {
                os: backend.target_os(),
                distribution: Some("ubuntu".into()),
                version: Some("24.04".into()),
                architecture: Some("x86_64".into()),
                kernel: None,
                abi: Some("linux-gnu".into()),
                image: None,
            },
            if backend == BackendKind::LinuxVps {
                BackendSpec::remote(backend, "ssh://vps.example")
            } else {
                BackendSpec::local(backend)
            },
            IdentityProfile {
                hostname: "target-host".into(),
                username: "agent".into(),
                uid: 1000,
                gid: 1000,
                groups: vec!["agent".into()],
                shell: Some("/bin/bash".into()),
                umask: Some(0o022),
                capabilities: BTreeSet::new(),
                passwd: None,
            },
            ClockProfile {
                timezone: "Asia/Shanghai".into(),
                tzdata: "2026a".into(),
                anchor_epoch_seconds: 1_788_652_800,
                rate: 1.0,
                monotonic_anchor_ns: None,
                boottime_anchor_ns: None,
                tzdata_sha256: Some("sha256:test".into()),
                allow_backend_tzdata: false,
                offset_seconds: Some(8 * 3600),
                follow_realtime: false,
            },
            LocaleProfile {
                lang: "en_US.UTF-8".into(),
                lc_all: "en_US.UTF-8".into(),
                lc_time: None,
                charset: Some("UTF-8".into()),
                icu_version: None,
                artifact: None,
            },
            PathProfile {
                cwd: "/workspace/project".into(),
                home: "/home/agent".into(),
                tmp: "/tmp".into(),
                workspace: Some("/workspace/project".into()),
                proc: Some("/proc".into()),
                sys: Some("/sys".into()),
                mounts: Some("/proc/self/mountinfo".into()),
                self_exe: Some("/proc/self/exe".into()),
                hidden_host_markers: vec![],
            },
        )
        .expect("valid profile")
    }

    fn complete(backend: BackendKind) -> BackendCapabilities {
        let mut capabilities = BackendCapabilities::new(backend);
        for probe in required_probe_set() {
            capabilities = capabilities.record(probe, ProbeEvidence::native());
        }
        capabilities
    }

    #[test]
    fn wsl_is_linux_but_carrier_is_separate() {
        assert_eq!(BackendKind::Wsl2.target_os(), TargetOs::Linux);
        assert_eq!(BackendKind::Wsl2.carrier(), "wsl2");
    }

    #[test]
    fn complete_native_capabilities_pass() {
        let p = profile(BackendKind::Wsl2);
        let report = p.evaluate(&complete(BackendKind::Wsl2));
        assert_eq!(report.decision, GateDecision::Equivalent);
    }

    #[test]
    fn missing_or_host_visible_required_probe_blocks() {
        let p = profile(BackendKind::Wsl2);
        let capabilities = complete(BackendKind::Wsl2)
            .record(ProbeId::PlatformKernel, ProbeEvidence::host_visible());
        let report = p.evaluate(&capabilities);
        assert_eq!(report.decision, GateDecision::Blocked);
        assert!(!report.probes[&ProbeId::PlatformKernel].passes);
    }

    #[test]
    fn explicitly_allowed_host_visible_probe_can_pass() {
        let mut p = profile(BackendKind::LinuxNative);
        p.policy.allowed_host_visible.insert(ProbeId::Timezone);
        p.policy.commitment = CommitmentLevel::RealBackend;
        let capabilities = complete(BackendKind::LinuxNative)
            .record(ProbeId::Timezone, ProbeEvidence::host_visible());
        assert_eq!(p.evaluate(&capabilities).decision, GateDecision::Equivalent);
    }

    #[test]
    fn default_contract_requires_every_known_probe() {
        assert_eq!(required_probe_set().len(), ProbeId::ALL.len());
        assert!(
            ProbeId::ALL
                .iter()
                .all(|probe| required_probe_set().contains(probe))
        );
    }

    #[test]
    fn every_required_failure_mode_blocks_observational_contract() {
        let p = profile(BackendKind::LinuxNative);
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
            let capabilities =
                complete(BackendKind::LinuxNative).record(ProbeId::PlatformKernel, evidence);
            assert_eq!(
                p.evaluate(&capabilities).decision,
                GateDecision::Blocked,
                "state {state:?} must block"
            );
        }
    }

    #[test]
    fn brokered_evidence_is_accepted_but_system_commitment_rejects_overlay() {
        let p = profile(BackendKind::LinuxNative);
        let capabilities = complete(BackendKind::LinuxNative)
            .record(ProbeId::FilesystemProc, ProbeEvidence::brokered());
        assert_eq!(p.evaluate(&capabilities).decision, GateDecision::Equivalent);

        let mut system = p.clone();
        system.policy.commitment = CommitmentLevel::SystemEquivalent;
        let overlay = complete(BackendKind::LinuxNative)
            .record(ProbeId::FilesystemProc, ProbeEvidence::virtualised());
        assert_eq!(system.evaluate(&overlay).decision, GateDecision::Blocked);
    }

    #[test]
    fn optional_probe_failure_is_degraded_and_not_equivalent() {
        let mut p = profile(BackendKind::LinuxNative);
        p.policy.required.remove(&ProbeId::ResourceMemory);
        p.policy.optional.insert(ProbeId::ResourceMemory);
        let capabilities = complete(BackendKind::LinuxNative)
            .record(ProbeId::ResourceMemory, ProbeEvidence::unknown());
        let report = p.evaluate(&capabilities);
        assert_eq!(report.decision, GateDecision::Degraded);
        assert_eq!(report.optional_failures, 1);
    }

    #[test]
    fn carrier_host_leak_blocks_even_when_legacy_allowlist_is_set() {
        let mut p = profile(BackendKind::Wsl2);
        p.policy.allowed_host_visible.insert(ProbeId::WslProvenance);
        p.policy.required.insert(ProbeId::WslProvenance);
        let capabilities = complete(BackendKind::Wsl2)
            .record(ProbeId::WslProvenance, ProbeEvidence::host_visible());
        assert_eq!(p.evaluate(&capabilities).decision, GateDecision::Blocked);
    }

    #[test]
    fn target_view_excludes_backend_endpoint_and_carrier() {
        let p = EnvironmentProfile::new(
            "vps-view",
            TargetPlatform {
                os: TargetOs::Linux,
                distribution: Some("debian".into()),
                version: Some("13".into()),
                architecture: Some("x86_64".into()),
                kernel: None,
                abi: Some("linux-gnu".into()),
                image: None,
            },
            BackendSpec::remote(BackendKind::LinuxVps, "ssh://private.example"),
            IdentityProfile {
                hostname: "target".into(),
                username: "agent".into(),
                uid: 1000,
                gid: 1000,
                groups: vec![],
                shell: None,
                umask: None,
                capabilities: BTreeSet::new(),
                passwd: None,
            },
            ClockProfile {
                timezone: "UTC".into(),
                tzdata: "2026a".into(),
                anchor_epoch_seconds: 0,
                rate: 1.0,
                monotonic_anchor_ns: None,
                boottime_anchor_ns: None,
                tzdata_sha256: Some("sha256:test".into()),
                allow_backend_tzdata: false,
                offset_seconds: Some(0),
                follow_realtime: false,
            },
            LocaleProfile {
                lang: "C".into(),
                lc_all: "C".into(),
                lc_time: None,
                charset: None,
                icu_version: None,
                artifact: None,
            },
            PathProfile {
                cwd: "/workspace".into(),
                home: "/home/agent".into(),
                tmp: "/tmp".into(),
                workspace: Some("/workspace".into()),
                proc: Some("/proc".into()),
                sys: Some("/sys".into()),
                mounts: Some("/proc/self/mountinfo".into()),
                self_exe: Some("/proc/self/exe".into()),
                hidden_host_markers: vec!["/srv/host".into()],
            },
        )
        .unwrap();
        let json = serde_json::to_string(&p.agent_view()).unwrap();
        assert!(!json.contains("linux-vps"));
        assert!(!json.contains("private.example"));
        assert!(!json.contains("backend"));
    }

    #[test]
    fn backend_tzdata_must_be_explicitly_allowed() {
        let mut p = profile(BackendKind::LinuxNative);
        p.clock.tzdata = "backend".into();
        assert!(matches!(
            p.validate(),
            Err(ProfileError::UnpinnedTimezoneData)
        ));
        p.clock.allow_backend_tzdata = true;
        assert!(p.validate().is_ok());
    }

    #[test]
    fn invalid_paths_and_mac_architecture_are_rejected() {
        let mut p = profile(BackendKind::LinuxNative);
        p.paths.cwd = "relative".into();
        assert!(matches!(
            p.validate(),
            Err(ProfileError::RelativePath("cwd"))
        ));

        let mut mac = profile(BackendKind::MacosNative);
        mac.target.architecture = None;
        assert!(matches!(
            mac.validate(),
            Err(ProfileError::MissingArchitecture)
        ));
    }

    #[test]
    fn backend_mismatch_blocks_even_when_probes_match() {
        let p = profile(BackendKind::MacosNative);
        assert_eq!(
            p.evaluate(&complete(BackendKind::LinuxNative)).decision,
            GateDecision::Blocked
        );
    }

    #[test]
    fn target_backend_os_mismatch_is_rejected() {
        let result = EnvironmentProfile::new(
            "bad",
            TargetPlatform {
                os: TargetOs::Macos,
                distribution: None,
                version: None,
                architecture: None,
                kernel: None,
                abi: None,
                image: None,
            },
            BackendSpec::local(BackendKind::Wsl2),
            IdentityProfile {
                hostname: "h".into(),
                username: "u".into(),
                uid: 1,
                gid: 1,
                groups: vec![],
                shell: None,
                umask: None,
                capabilities: BTreeSet::new(),
                passwd: None,
            },
            ClockProfile {
                timezone: "UTC".into(),
                tzdata: "2026a".into(),
                anchor_epoch_seconds: 0,
                rate: 1.0,
                monotonic_anchor_ns: None,
                boottime_anchor_ns: None,
                tzdata_sha256: Some("sha256:test".into()),
                allow_backend_tzdata: false,
                offset_seconds: Some(0),
                follow_realtime: false,
            },
            LocaleProfile {
                lang: "C".into(),
                lc_all: "C".into(),
                lc_time: None,
                charset: None,
                icu_version: None,
                artifact: None,
            },
            PathProfile {
                cwd: "/".into(),
                home: "/".into(),
                tmp: "/tmp".into(),
                workspace: None,
                proc: Some("/proc".into()),
                sys: Some("/sys".into()),
                mounts: Some("/proc/self/mountinfo".into()),
                self_exe: Some("/proc/self/exe".into()),
                hidden_host_markers: vec![],
            },
        );
        assert!(matches!(result, Err(ProfileError::OsMismatch { .. })));
    }

    #[test]
    fn stale_deserialised_contract_cannot_pass_gate() {
        let mut p = profile(BackendKind::LinuxNative);
        p.contract = "environment-contract-v0".into();
        let report = p.evaluate(&complete(BackendKind::LinuxNative));
        assert!(!report.profile_valid);
        assert_eq!(report.decision, GateDecision::Blocked);
    }

    #[test]
    fn vps_requires_a_remote_endpoint() {
        let mut result = profile(BackendKind::LinuxVps);
        result.backend.endpoint = None;
        assert!(matches!(
            result.validate(),
            Err(ProfileError::MissingRemoteEndpoint)
        ));
    }
}
