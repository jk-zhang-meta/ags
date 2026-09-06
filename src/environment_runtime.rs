//! Execution-facing pieces of the environment contract.
//!
//! This module intentionally stays above the operating-system boundary.  It
//! does not intercept syscalls and it never executes a command.  It provides
//! the deterministic pieces an actual launcher/broker can use: profile
//! loading, carrier detection, a scrubbed agent environment, command
//! classification, and virtual responses for the small observation set that
//! AGS can answer without consulting the host.

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::environment::{BackendKind, EnvironmentProfile, TargetOs};

/// Version of the line-oriented broker protocol.  A protocol version is part
/// of every request and response so an adapter cannot accidentally treat a
/// newer request as if it had the old fail-closed semantics.
pub const ENVIRONMENT_BROKER_PROTOCOL_V1: &str = "environment-broker-v1";
pub const MAX_BROKER_LINE_BYTES: usize = 1_048_576;

/// A profile loaded from JSON and validated before use.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeProfile {
    pub profile: EnvironmentProfile,
}

impl RuntimeProfile {
    pub fn from_json(input: &str) -> Result<Self, RuntimeError> {
        let profile: EnvironmentProfile = serde_json::from_str(input)
            .map_err(|error| RuntimeError::InvalidProfile(error.to_string()))?;
        profile
            .validate()
            .map_err(|error| RuntimeError::InvalidProfile(error.to_string()))?;
        validate_profile_strings(&profile)
            .map_err(|error| RuntimeError::InvalidProfile(error.to_string()))?;
        Ok(Self { profile })
    }
}

fn validate_profile_strings(profile: &EnvironmentProfile) -> Result<(), RuntimeError> {
    let fields = [
        ("profile id", profile.id.as_str()),
        ("hostname", profile.identity.hostname.as_str()),
        ("username", profile.identity.username.as_str()),
        ("timezone", profile.clock.timezone.as_str()),
        ("cwd", profile.paths.cwd.as_str()),
        ("home", profile.paths.home.as_str()),
        ("tmp", profile.paths.tmp.as_str()),
        ("LANG", profile.locale.lang.as_str()),
        ("LC_ALL", profile.locale.lc_all.as_str()),
    ];
    for (name, value) in fields {
        if value.is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n'))
        {
            return Err(RuntimeError::InvalidProfile(format!(
                "{name} is empty or contains a control character"
            )));
        }
    }
    if let Some(value) = profile.locale.lc_time.as_deref()
        && (value.is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n')))
    {
        return Err(RuntimeError::InvalidProfile(
            "LC_TIME is empty or contains a control character".to_string(),
        ));
    }
    Ok(())
}

/// Facts about the actual carrier.  These are audit/routing data and must not
/// be copied into agent-facing environment variables or prompt metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendDetection {
    pub kind: BackendKind,
    pub origin: ValueOrigin,
    pub evidence: String,
}

/// Detect the current carrier without attempting to infer a VPS from weak
/// heuristics.  Operators can provide `AGS_BACKEND_KIND=linux-vps` when the
/// deployment knows it is a VPS; otherwise Linux is conservatively native.
pub fn detect_backend() -> BackendDetection {
    if let Some(value) =
        std::env::var_os("AGS_BACKEND_KIND").and_then(|value| value.into_string().ok())
        && let Some(kind) = parse_backend_kind(&value)
    {
        return BackendDetection {
            kind,
            origin: ValueOrigin::HostSanitized,
            evidence: "AGS_BACKEND_KIND (operator supplied)".to_string(),
        };
    }

    #[cfg(target_os = "macos")]
    {
        return BackendDetection {
            kind: BackendKind::MacosNative,
            origin: ValueOrigin::HostSanitized,
            evidence: "compile-time target_os=macos".to_string(),
        };
    }

    #[cfg(target_os = "linux")]
    {
        let wsl_env = std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some();
        let wsl_kernel = fs::read_to_string("/proc/version")
            .map(|text| {
                let lower = text.to_ascii_lowercase();
                lower.contains("microsoft") || lower.contains("wsl")
            })
            .unwrap_or(false);
        if wsl_env || wsl_kernel {
            return BackendDetection {
                kind: BackendKind::Wsl2,
                origin: ValueOrigin::HostSanitized,
                evidence: "WSL environment/kernel probe (carrier only)".to_string(),
            };
        }
        return BackendDetection {
            kind: BackendKind::LinuxNative,
            origin: ValueOrigin::HostSanitized,
            evidence: "Linux without an explicit WSL marker".to_string(),
        };
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        BackendDetection {
            kind: BackendKind::LinuxNative,
            origin: ValueOrigin::Unknown,
            evidence: "unsupported host target".to_string(),
        }
    }
}

fn parse_backend_kind(value: &str) -> Option<BackendKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "linux-native" | "linux" | "native" => Some(BackendKind::LinuxNative),
        "wsl2" | "wsl" => Some(BackendKind::Wsl2),
        "linux-vps" | "vps" => Some(BackendKind::LinuxVps),
        "macos-native" | "macos" | "darwin" => Some(BackendKind::MacosNative),
        _ => None,
    }
}

/// One environment value with provenance.  `HostPassthrough` is deliberately
/// absent from the default allowlist; callers must opt into it explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentValue {
    pub value: String,
    pub origin: ValueOrigin,
    pub state: ValueState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueOrigin {
    Profile,
    HostSanitized,
    HostPassthrough,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueState {
    Virtual,
    Native,
    HostVisible,
    Redacted,
    Unknown,
}

/// Agent-facing environment.  Carrier variables are removed before this
/// structure is returned; the removed names remain only as audit metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEnvironment {
    pub values: BTreeMap<String, EnvironmentValue>,
    pub removed_host_keys: Vec<String>,
}

impl AgentEnvironment {
    /// Build an allowlisted environment from a host snapshot.  Arbitrary host
    /// values are never copied; only harmless terminal hints survive.
    pub fn from_host(
        profile: &EnvironmentProfile,
        host: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut values = BTreeMap::new();
        let mut removed = Vec::new();

        for (key, value) in host {
            if is_safe_inherited_key(&key) {
                values.insert(
                    key,
                    EnvironmentValue {
                        value,
                        origin: ValueOrigin::HostSanitized,
                        state: ValueState::Native,
                    },
                );
            } else {
                removed.push(key);
            }
        }

        let target_path = match profile.target.os {
            TargetOs::Linux => "/usr/local/bin:/usr/bin:/bin",
            TargetOs::Macos => "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        };
        insert_profile(&mut values, "HOME", &profile.paths.home);
        insert_profile(&mut values, "PWD", &profile.paths.cwd);
        insert_profile(&mut values, "OLDPWD", &profile.paths.cwd);
        insert_profile(&mut values, "TMPDIR", &profile.paths.tmp);
        insert_profile(&mut values, "USER", &profile.identity.username);
        insert_profile(&mut values, "LOGNAME", &profile.identity.username);
        insert_profile(&mut values, "HOSTNAME", &profile.identity.hostname);
        insert_profile(&mut values, "LANG", &profile.locale.lang);
        insert_profile(&mut values, "LC_ALL", &profile.locale.lc_all);
        if let Some(lc_time) = &profile.locale.lc_time {
            insert_profile(&mut values, "LC_TIME", lc_time);
        }
        insert_profile(&mut values, "TZ", &profile.clock.timezone);
        insert_profile(&mut values, "PATH", target_path);
        values.insert(
            "SHELL".to_string(),
            EnvironmentValue {
                value: profile.identity.shell.clone().unwrap_or_else(|| {
                    match profile.target.os {
                        TargetOs::Linux => "/bin/bash",
                        TargetOs::Macos => "/bin/zsh",
                    }
                    .to_string()
                }),
                origin: ValueOrigin::Profile,
                state: ValueState::Virtual,
            },
        );
        removed.sort();
        removed.dedup();
        Self {
            values,
            removed_host_keys: removed,
        }
    }

    pub fn as_map(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(key, value)| (key.clone(), value.value.clone()))
            .collect()
    }
}

fn insert_profile(values: &mut BTreeMap<String, EnvironmentValue>, key: &str, value: &str) {
    values.insert(
        key.to_string(),
        EnvironmentValue {
            value: value.to_string(),
            origin: ValueOrigin::Profile,
            state: ValueState::Virtual,
        },
    );
}

fn is_safe_inherited_key(key: &str) -> bool {
    matches!(key, "TERM" | "COLORTERM" | "NO_COLOR")
}

/// A deliberately small command classification.  Shell strings and dynamic
/// `bash -c` scripts are always `Unknown`; callers must not classify them by
/// substring matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandClassification {
    VirtualObservation(VirtualObservationAudit),
    Passthrough(PassthroughAudit),
    Unknown(UnknownAudit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Hostname,
    Whoami,
    Id,
    Uname,
    Pwd,
    Date,
    Env,
    Locale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualObservationAudit {
    pub kind: ObservationKind,
    pub origin: ValueOrigin,
    pub state: ValueState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassthroughAudit {
    pub argv: Vec<String>,
    pub origin: ValueOrigin,
    pub state: ValueState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownAudit {
    pub argv: Vec<String>,
    pub origin: ValueOrigin,
    pub state: ValueState,
}

pub fn classify_command(argv: &[String]) -> CommandClassification {
    let Some(command) = argv.first().map(|value| basename(value)) else {
        return CommandClassification::Unknown(UnknownAudit {
            argv: Vec::new(),
            origin: ValueOrigin::Unknown,
            state: ValueState::Unknown,
        });
    };
    let observation = match command {
        "hostname" => Some(ObservationKind::Hostname),
        "whoami" => Some(ObservationKind::Whoami),
        "id" => Some(ObservationKind::Id),
        "uname" => Some(ObservationKind::Uname),
        "pwd" => Some(ObservationKind::Pwd),
        "date" => Some(ObservationKind::Date),
        "env" | "printenv" => Some(ObservationKind::Env),
        "locale" => Some(ObservationKind::Locale),
        _ => None,
    };
    if let Some(kind) = observation {
        if supported_observation_argv(kind, argv) {
            return CommandClassification::VirtualObservation(VirtualObservationAudit {
                kind,
                origin: ValueOrigin::Profile,
                state: ValueState::Virtual,
            });
        }
        return CommandClassification::Unknown(UnknownAudit {
            argv: argv.to_vec(),
            origin: ValueOrigin::Unknown,
            state: ValueState::Unknown,
        });
    }
    if matches!(
        command,
        "bash" | "sh" | "zsh" | "fish" | "powershell" | "pwsh"
    ) {
        return CommandClassification::Unknown(UnknownAudit {
            argv: argv.to_vec(),
            origin: ValueOrigin::Unknown,
            state: ValueState::Unknown,
        });
    }
    CommandClassification::Passthrough(PassthroughAudit {
        argv: argv.to_vec(),
        origin: ValueOrigin::HostPassthrough,
        state: ValueState::HostVisible,
    })
}

/// Keep the virtual surface intentionally explicit.  If a caller asks for an
/// option whose semantics are not implemented by `virtual_observation`, the
/// request becomes unknown and is fail-closed instead of receiving a plausible
/// but incorrect answer.
fn supported_observation_argv(kind: ObservationKind, argv: &[String]) -> bool {
    match kind {
        ObservationKind::Hostname
        | ObservationKind::Whoami
        | ObservationKind::Id
        | ObservationKind::Env
        | ObservationKind::Locale => argv.len() == 1,
        ObservationKind::Pwd => argv.len() == 1 || argv[1..].iter().all(|arg| arg == "-L"),
        ObservationKind::Uname => {
            if argv.len() == 1 {
                return true;
            }
            argv.iter().skip(1).all(|arg| {
                arg == "-a"
                    || (!arg.is_empty()
                        && arg.starts_with('-')
                        && arg[1..]
                            .chars()
                            .all(|flag| matches!(flag, 's' | 'n' | 'r' | 'm')))
            })
        }
        ObservationKind::Date => argv.iter().skip(1).all(|arg| {
            arg == "-u"
                || arg == "--utc"
                || (arg.starts_with('+') && arg.len() > 1 && !arg.contains('\0'))
        }),
    }
}

fn basename(value: &str) -> &str {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
}

/// A response produced entirely from the profile.  It is a result object,
/// not an execution request; passthrough commands are never run here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationResult {
    pub kind: ObservationKind,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub origin: ValueOrigin,
    pub state: ValueState,
}

pub fn virtual_observation(
    profile: &EnvironmentProfile,
    kind: ObservationKind,
    argv: &[String],
) -> ObservationResult {
    let (stdout, stderr, exit_code) = match kind {
        ObservationKind::Hostname => (format!("{}\n", profile.identity.hostname), String::new(), 0),
        ObservationKind::Whoami => (format!("{}\n", profile.identity.username), String::new(), 0),
        ObservationKind::Id => (
            format!(
                "uid={}({}) gid={}({}) groups={}({})\n",
                profile.identity.uid,
                profile.identity.username,
                profile.identity.gid,
                profile.identity.username,
                profile.identity.gid,
                profile.identity.username
            ),
            String::new(),
            0,
        ),
        ObservationKind::Pwd => (format!("{}\n", profile.paths.cwd), String::new(), 0),
        ObservationKind::Env => {
            let env = AgentEnvironment::from_host(profile, std::iter::empty());
            let output = env
                .as_map()
                .into_iter()
                .map(|(key, value)| format!("{key}={value}\n"))
                .collect();
            (output, String::new(), 0)
        }
        ObservationKind::Locale => (
            format!(
                "LANG={}\nLC_ALL={}\nLC_TIME={}\n",
                profile.locale.lang,
                profile.locale.lc_all,
                profile
                    .locale
                    .lc_time
                    .as_deref()
                    .unwrap_or(profile.locale.lc_all.as_str())
            ),
            String::new(),
            0,
        ),
        ObservationKind::Uname => uname_output(profile, argv),
        ObservationKind::Date => date_output(profile, argv),
    };
    let (origin, state) = if exit_code == 0 {
        (ValueOrigin::Profile, ValueState::Virtual)
    } else {
        (ValueOrigin::Unknown, ValueState::Unknown)
    };
    ObservationResult {
        kind,
        stdout,
        stderr,
        exit_code,
        origin,
        state,
    }
}

fn uname_output(profile: &EnvironmentProfile, argv: &[String]) -> (String, String, i32) {
    let os = match profile.target.os {
        TargetOs::Linux => "Linux",
        TargetOs::Macos => "Darwin",
    };
    let release = profile.target.version.as_deref().unwrap_or("ags-virtual");
    let machine = profile.target.architecture.as_deref().unwrap_or("x86_64");
    let mut fields = Vec::new();
    let mut explicit = false;
    for arg in argv.iter().skip(1) {
        if arg == "-a" {
            fields = vec![os, profile.identity.hostname.as_str(), release, "", machine];
            explicit = true;
            break;
        }
        for flag in arg.strip_prefix('-').unwrap_or("").chars() {
            let field = match flag {
                's' => Some(os),
                'n' => Some(profile.identity.hostname.as_str()),
                'r' => Some(release),
                'm' => Some(machine),
                _ => None,
            };
            if let Some(field) = field {
                fields.push(field);
                explicit = true;
            }
        }
    }
    if !explicit {
        fields.push(os);
    }
    (format!("{}\n", fields.join(" ")), String::new(), 0)
}

fn date_output(profile: &EnvironmentProfile, argv: &[String]) -> (String, String, i32) {
    let epoch = if profile.clock.follow_realtime {
        Utc::now().timestamp()
    } else {
        profile.clock.anchor_epoch_seconds
    };
    let Some(now) = DateTime::<Utc>::from_timestamp(epoch, 0) else {
        return (
            String::new(),
            "date: profile anchor is outside the supported range\n".to_string(),
            2,
        );
    };
    let utc = argv.iter().any(|arg| arg == "-u" || arg == "--utc");
    let format = argv
        .iter()
        .find_map(|arg| arg.strip_prefix('+'))
        .unwrap_or("%a %b %e %H:%M:%S %Z %Y");
    if utc {
        let rendered = now.format(format).to_string();
        return (format!("{rendered}\n"), String::new(), 0);
    }
    let Some(offset) = timezone_offset(&profile.clock) else {
        return (
            String::new(),
            "date: profile timezone requires a pinned timezone database\n".to_string(),
            2,
        );
    };
    let Some(offset) = FixedOffset::east_opt(offset.num_seconds() as i32) else {
        return (
            String::new(),
            "date: profile timezone offset is invalid\n".to_string(),
            2,
        );
    };
    let rendered = now.with_timezone(&offset).format(format).to_string();
    (format!("{rendered}\n"), String::new(), 0)
}

fn timezone_offset(clock: &crate::environment::ClockProfile) -> Option<Duration> {
    if let Some(seconds) = clock.offset_seconds {
        return Some(Duration::seconds(seconds));
    }
    let timezone = clock.timezone.as_str();
    if matches!(timezone, "UTC" | "GMT" | "Etc/UTC") {
        return Some(Duration::zero());
    }
    let value = timezone
        .strip_prefix("UTC")
        .or_else(|| timezone.strip_prefix("GMT"));
    let value = match value {
        Some(value) => value,
        None => timezone
            .strip_prefix('+')
            .or_else(|| timezone.strip_prefix('-'))?,
    };
    let sign = if timezone.contains('-') { -1 } else { 1 };
    let (hours, minutes) = if let Some((hours, minutes)) = value.split_once(':') {
        (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?)
    } else {
        (value.parse::<i64>().ok()?, 0)
    };
    Some(Duration::minutes(sign * (hours * 60 + minutes)))
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid environment profile: {0}")]
    InvalidProfile(String),
}

/// Versioned request accepted by [`EnvironmentBroker`].  The JSONL transport
/// intentionally carries argv as an array: a shell string would reintroduce
/// quoting, expansion, and command-substitution semantics before AGS can
/// classify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerRequest {
    #[serde(default = "default_broker_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    /// A second, per-request gate.  Even when the broker is configured to
    /// permit passthrough, the caller must explicitly set this field.
    #[serde(default)]
    pub allow_host_passthrough: bool,
    /// Optional upper bounds requested by the caller.  The broker clamps them
    /// to its own limits before spawning anything.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

fn default_broker_protocol() -> String {
    ENVIRONMENT_BROKER_PROTOCOL_V1.to_string()
}

impl BrokerRequest {
    pub fn new(argv: impl IntoIterator<Item = String>) -> Self {
        Self {
            protocol: default_broker_protocol(),
            id: None,
            agent: None,
            argv: argv.into_iter().collect(),
            allow_host_passthrough: false,
            timeout_ms: None,
            max_output_bytes: None,
        }
    }

    pub fn for_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn allow_host_passthrough(mut self) -> Self {
        self.allow_host_passthrough = true;
        self
    }
}

/// Broker-level limits and policy.  The default permits only a request that
/// explicitly opts into host passthrough; unknown/dynamic argv never execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    pub host_passthrough_enabled: bool,
    pub timeout: StdDuration,
    pub max_output_bytes: usize,
    pub cwd: Option<std::path::PathBuf>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            host_passthrough_enabled: true,
            timeout: StdDuration::from_secs(30),
            max_output_bytes: 1024 * 1024,
            cwd: None,
        }
    }
}

impl BrokerConfig {
    pub fn deny_host_passthrough(mut self) -> Self {
        self.host_passthrough_enabled = false;
        self
    }

    pub fn with_timeout(mut self, timeout: StdDuration) -> Self {
        self.timeout = timeout.max(StdDuration::from_millis(1));
        self
    }

    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes.max(1);
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerClassification {
    VirtualObservation,
    HostPassthrough,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerDecision {
    Virtual,
    Executed,
    Blocked,
}

/// The complete, machine-readable audit record emitted for every request.
/// `origin` and `state` make it impossible for a downstream adapter to mistake
/// a host-visible command for a virtual observation without explicitly
/// handling that state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerAudit {
    pub argv: Vec<String>,
    pub classification: BrokerClassification,
    #[serde(default)]
    pub observation: Option<ObservationKind>,
    pub decision: BrokerDecision,
    pub origin: ValueOrigin,
    pub state: ValueState,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerExecution {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_truncated: bool,
}

/// One response line.  Virtual observations use `observation`; explicit
/// passthrough uses `execution`; blocked requests carry only the audit/error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerResponse {
    pub protocol: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    /// Whether the result satisfies the profile's agent-visible contract.
    /// Successful host passthrough deliberately has `ok=true` but
    /// `equivalent=false` because its output remains host-visible.
    pub equivalent: bool,
    pub ok: bool,
    pub audit: BrokerAudit,
    #[serde(default)]
    pub observation: Option<ObservationResult>,
    #[serde(default)]
    pub execution: Option<BrokerExecution>,
}

impl BrokerResponse {
    fn blocked_request(request: &BrokerRequest, error: impl Into<String>) -> Self {
        Self::blocked_with_audit(
            request,
            BrokerClassification::Unknown,
            ValueOrigin::Unknown,
            ValueState::Unknown,
            error,
        )
    }

    fn blocked_with_audit(
        request: &BrokerRequest,
        classification: BrokerClassification,
        origin: ValueOrigin,
        state: ValueState,
        error: impl Into<String>,
    ) -> Self {
        let error = error.into();
        Self {
            protocol: default_broker_protocol(),
            id: request.id.clone(),
            agent: request.agent.clone(),
            equivalent: false,
            ok: false,
            audit: BrokerAudit {
                argv: request.argv.clone(),
                classification,
                observation: None,
                decision: BrokerDecision::Blocked,
                origin,
                state,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                error: Some(error),
            },
            observation: None,
            execution: None,
        }
    }
}

/// A reusable broker seam for launchers, provider adapters, and hook bridges.
/// It owns classification and execution policy so callers cannot accidentally
/// run a dynamic shell command while trying to answer an observation.
#[derive(Debug, Clone)]
pub struct EnvironmentBroker {
    profile: EnvironmentProfile,
    config: BrokerConfig,
}

impl EnvironmentBroker {
    pub fn new(profile: EnvironmentProfile) -> Result<Self, RuntimeError> {
        profile
            .validate()
            .map_err(|error| RuntimeError::InvalidProfile(error.to_string()))?;
        validate_profile_strings(&profile)?;
        Ok(Self {
            profile,
            config: BrokerConfig::default(),
        })
    }

    pub fn with_config(mut self, config: BrokerConfig) -> Self {
        self.config = config;
        self
    }

    pub fn profile(&self) -> &EnvironmentProfile {
        &self.profile
    }

    pub fn handle(&self, request: BrokerRequest) -> BrokerResponse {
        if request.protocol != ENVIRONMENT_BROKER_PROTOCOL_V1 {
            return BrokerResponse::blocked_request(
                &request,
                format!("unsupported broker protocol `{}`", request.protocol),
            );
        }
        match classify_command(&request.argv) {
            CommandClassification::VirtualObservation(audit) => {
                let result = virtual_observation(&self.profile, audit.kind, &request.argv);
                let successful = result.exit_code == 0 && result.state == ValueState::Virtual;
                BrokerResponse {
                    protocol: default_broker_protocol(),
                    id: request.id,
                    agent: request.agent,
                    equivalent: successful,
                    ok: successful,
                    audit: BrokerAudit {
                        argv: request.argv,
                        classification: BrokerClassification::VirtualObservation,
                        observation: Some(audit.kind),
                        decision: if successful {
                            BrokerDecision::Virtual
                        } else {
                            BrokerDecision::Blocked
                        },
                        origin: result.origin,
                        state: result.state,
                        exit_code: Some(result.exit_code),
                        timed_out: false,
                        output_truncated: false,
                        error: (!successful).then(|| result.stderr.trim().to_string()),
                    },
                    observation: Some(result),
                    execution: None,
                }
            }
            CommandClassification::Unknown(audit) => BrokerResponse {
                protocol: default_broker_protocol(),
                id: request.id,
                agent: request.agent,
                equivalent: false,
                ok: false,
                audit: BrokerAudit {
                    argv: audit.argv,
                    classification: BrokerClassification::Unknown,
                    observation: None,
                    decision: BrokerDecision::Blocked,
                    origin: audit.origin,
                    state: audit.state,
                    exit_code: None,
                    timed_out: false,
                    output_truncated: false,
                    error: Some("unknown or dynamic command is fail-closed".to_string()),
                },
                observation: None,
                execution: None,
            },
            CommandClassification::Passthrough(audit) => {
                if !request.allow_host_passthrough {
                    return BrokerResponse {
                        protocol: default_broker_protocol(),
                        id: request.id,
                        agent: request.agent,
                        equivalent: false,
                        ok: false,
                        audit: BrokerAudit {
                            argv: audit.argv,
                            classification: BrokerClassification::HostPassthrough,
                            observation: None,
                            decision: BrokerDecision::Blocked,
                            origin: audit.origin,
                            state: audit.state,
                            exit_code: None,
                            timed_out: false,
                            output_truncated: false,
                            error: Some(
                                "host passthrough requires allow_host_passthrough=true".to_string(),
                            ),
                        },
                        observation: None,
                        execution: None,
                    };
                }
                if !self.config.host_passthrough_enabled {
                    return BrokerResponse::blocked_with_audit(
                        &request,
                        BrokerClassification::HostPassthrough,
                        audit.origin,
                        audit.state,
                        "host passthrough is disabled by broker policy",
                    );
                }
                self.execute_passthrough(request, audit)
            }
        }
    }

    /// Serve newline-delimited JSON until EOF.  A malformed request produces a
    /// blocked response and does not terminate the stream, allowing a provider
    /// hook process to recover after one bad line.
    pub fn serve_jsonl<R: BufRead, W: Write>(
        &self,
        input: R,
        mut output: W,
    ) -> std::io::Result<BrokerStats> {
        let mut stats = BrokerStats::default();
        let mut input = input;
        loop {
            let Some(raw) = read_json_line_capped(&mut input, MAX_BROKER_LINE_BYTES)? else {
                break;
            };
            if raw.len() > MAX_BROKER_LINE_BYTES {
                stats.requests += 1;
                stats.malformed += 1;
                let response = BrokerResponse::blocked_request(
                    &BrokerRequest::new(std::iter::empty()),
                    format!("broker request exceeds {MAX_BROKER_LINE_BYTES} bytes"),
                );
                serde_json::to_writer(&mut output, &response).map_err(std::io::Error::other)?;
                output.write_all(b"\n")?;
                stats.responses += 1;
                continue;
            }
            let line = std::str::from_utf8(&raw).map(str::trim).unwrap_or_default();
            if line.is_empty() {
                continue;
            }
            stats.requests += 1;
            let response = match serde_json::from_str::<BrokerRequest>(line) {
                Ok(request) => self.handle(request),
                Err(error) => {
                    stats.malformed += 1;
                    BrokerResponse::blocked_request(
                        &BrokerRequest::new(std::iter::empty()),
                        format!("invalid broker request JSON: {error}"),
                    )
                }
            };
            serde_json::to_writer(&mut output, &response).map_err(std::io::Error::other)?;
            output.write_all(b"\n")?;
            stats.responses += 1;
        }
        output.flush()?;
        Ok(stats)
    }

    fn execute_passthrough(
        &self,
        request: BrokerRequest,
        audit: PassthroughAudit,
    ) -> BrokerResponse {
        let timeout = request
            .timeout_ms
            .map(StdDuration::from_millis)
            .unwrap_or(self.config.timeout)
            .min(self.config.timeout)
            .max(StdDuration::from_millis(1));
        let max_output = request
            .max_output_bytes
            .unwrap_or(self.config.max_output_bytes)
            .min(self.config.max_output_bytes)
            .max(1);
        let result = run_host_command(
            &self.profile,
            &request.argv,
            self.config.cwd.as_deref(),
            timeout,
            max_output,
        );
        match result {
            Ok(capture) => {
                let execution = BrokerExecution {
                    stdout: capture.stdout,
                    stderr: capture.stderr,
                    exit_code: capture.exit_code,
                    timed_out: capture.timed_out,
                    output_truncated: capture.output_truncated,
                };
                let successful = execution.exit_code == Some(0) && !execution.timed_out;
                BrokerResponse {
                    protocol: default_broker_protocol(),
                    id: request.id,
                    agent: request.agent,
                    equivalent: false,
                    ok: successful,
                    audit: BrokerAudit {
                        argv: audit.argv,
                        classification: BrokerClassification::HostPassthrough,
                        observation: None,
                        decision: BrokerDecision::Executed,
                        origin: ValueOrigin::HostPassthrough,
                        state: ValueState::HostVisible,
                        exit_code: execution.exit_code,
                        timed_out: execution.timed_out,
                        output_truncated: execution.output_truncated,
                        error: (!successful).then(|| {
                            if execution.timed_out {
                                "host command timed out".to_string()
                            } else {
                                "host command exited unsuccessfully".to_string()
                            }
                        }),
                    },
                    observation: None,
                    execution: Some(execution),
                }
            }
            Err(error) => BrokerResponse {
                protocol: default_broker_protocol(),
                id: request.id,
                agent: request.agent,
                equivalent: false,
                ok: false,
                audit: BrokerAudit {
                    argv: audit.argv,
                    classification: BrokerClassification::HostPassthrough,
                    observation: None,
                    decision: BrokerDecision::Blocked,
                    origin: ValueOrigin::HostPassthrough,
                    state: ValueState::HostVisible,
                    exit_code: None,
                    timed_out: false,
                    output_truncated: false,
                    error: Some(error),
                },
                observation: None,
                execution: None,
            },
        }
    }
}

/// Read one line while retaining at most `max + 1` bytes.  `BufRead::lines`
/// allocates the complete attacker-controlled line before a caller can reject
/// it; this helper drains an oversized line without retaining the tail.
fn read_json_line_capped<R: BufRead>(
    input: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(chunk.len(), |index| index + 1);
        if !oversized {
            let remaining = max.saturating_add(1).saturating_sub(line.len());
            let keep = remaining.min(consumed);
            line.extend_from_slice(&chunk[..keep]);
            oversized = line.len() > max;
        }
        input.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStats {
    pub requests: usize,
    pub responses: usize,
    pub malformed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandCapture {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
    output_truncated: bool,
}

fn run_host_command(
    profile: &EnvironmentProfile,
    argv: &[String],
    cwd: Option<&Path>,
    timeout: StdDuration,
    max_output_bytes: usize,
) -> Result<CommandCapture, String> {
    let Some(program) = argv.first() else {
        return Err("cannot execute an empty argv".to_string());
    };
    let host = std::env::vars();
    let agent_env = AgentEnvironment::from_host(profile, host);
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(agent_env.as_map())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let effective_cwd = cwd.unwrap_or_else(|| Path::new(&profile.paths.cwd));
    if !effective_cwd.is_dir() {
        return Err(format!(
            "profile working directory is unavailable: {}",
            effective_cwd.display()
        ));
    }
    command.current_dir(effective_cwd);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn host command: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture host command stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture host command stderr".to_string())?;
    let stdout_reader = thread::spawn(move || read_capped(stdout, max_output_bytes));
    let stderr_reader = thread::spawn(move || read_capped(stderr, max_output_bytes));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(StdDuration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed waiting for host command: {error}"));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked".to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked".to_string())?;
    Ok(CommandCapture {
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        output_truncated: stdout.truncated || stderr.truncated,
    })
}

struct CappedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_capped<R: Read>(mut reader: R, max: usize) -> CappedBytes {
    let mut bytes = Vec::with_capacity(max.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len() < max {
                    let keep = (max - bytes.len()).min(count);
                    bytes.extend_from_slice(&buffer[..keep]);
                    truncated |= keep < count;
                } else {
                    truncated = true;
                }
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    CappedBytes { bytes, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"{
      "contract":"environment-contract-v1", "id":"linux-test",
      "target":{"os":"linux","distribution":"ubuntu","version":"24.04","architecture":"x86_64"},
      "backend":{"kind":"wsl2"},
      "identity":{"hostname":"target-host","username":"agent","uid":1000,"gid":1000,"groups":["agent"]},
      "clock":{"timezone":"Asia/Shanghai","tzdata":"2026a","anchor_epoch_seconds":1788652800,"rate":1.0},
      "locale":{"lang":"en_US.UTF-8","lc_all":"en_US.UTF-8"},
      "paths":{"cwd":"/workspace/project","home":"/home/agent","tmp":"/tmp"}
    }"#;

    #[test]
    fn profile_json_is_validated() {
        let runtime = RuntimeProfile::from_json(PROFILE).expect("profile");
        assert_eq!(runtime.profile.backend.kind, BackendKind::Wsl2);
    }

    #[test]
    fn host_carrier_keys_are_removed_and_profile_wins() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let env = AgentEnvironment::from_host(
            &profile,
            [
                ("WSL_DISTRO_NAME".into(), "Ubuntu".into()),
                ("SSH_CONNECTION".into(), "secret".into()),
                ("PATH".into(), "C:\\Windows".into()),
                ("TERM".into(), "xterm-256color".into()),
            ],
        );
        assert_eq!(env.values["HOSTNAME"].value, "target-host");
        assert_eq!(env.values["PATH"].value, "/usr/local/bin:/usr/bin:/bin");
        assert!(!env.values.contains_key("WSL_DISTRO_NAME"));
        assert!(
            env.removed_host_keys
                .contains(&"SSH_CONNECTION".to_string())
        );
    }

    #[test]
    fn observation_commands_are_exact_argv_only() {
        let uname = classify_command(&["uname".into(), "-a".into()]);
        assert!(matches!(
            uname,
            CommandClassification::VirtualObservation(VirtualObservationAudit {
                kind: ObservationKind::Uname,
                ..
            })
        ));
        let dynamic = classify_command(&["bash".into(), "-c".into(), "uname".into()]);
        assert!(matches!(dynamic, CommandClassification::Unknown(_)));
        let unsupported = classify_command(&["hostname".into(), "--fqdn".into()]);
        assert!(matches!(unsupported, CommandClassification::Unknown(_)));
        let cargo = classify_command(&["cargo".into(), "test".into()]);
        assert!(matches!(cargo, CommandClassification::Passthrough(_)));
    }

    #[test]
    fn virtual_values_never_use_host_identity() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let result = virtual_observation(&profile, ObservationKind::Hostname, &["hostname".into()]);
        assert_eq!(result.stdout, "target-host\n");
        assert_eq!(result.origin, ValueOrigin::Profile);
        assert_eq!(result.state, ValueState::Virtual);
    }

    #[test]
    fn fixed_offset_timezone_is_deterministic() {
        let mut clock = RuntimeProfile::from_json(PROFILE).unwrap().profile.clock;
        clock.timezone = "UTC+08:00".into();
        clock.offset_seconds = None;
        assert_eq!(timezone_offset(&clock), Some(Duration::hours(8)));
        clock.timezone = "-05:30".into();
        assert_eq!(timezone_offset(&clock), Some(Duration::minutes(-330)));
        clock.timezone = "Asia/Shanghai".into();
        assert_eq!(timezone_offset(&clock), None);
        clock.offset_seconds = Some(8 * 3600);
        assert_eq!(timezone_offset(&clock), Some(Duration::hours(8)));
    }

    #[test]
    fn broker_answers_virtual_observation_with_audit() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let broker = EnvironmentBroker::new(profile).unwrap();
        let response = broker.handle(
            BrokerRequest::new(["hostname".into()])
                .with_id("r-1")
                .for_agent("codex"),
        );
        assert!(response.ok);
        assert!(response.equivalent);
        assert_eq!(response.id.as_deref(), Some("r-1"));
        assert_eq!(response.agent.as_deref(), Some("codex"));
        assert_eq!(response.audit.decision, BrokerDecision::Virtual);
        assert_eq!(response.audit.state, ValueState::Virtual);
        assert_eq!(response.observation.unwrap().stdout, "target-host\n");
    }

    #[test]
    fn broker_blocks_unknown_even_when_passthrough_is_requested() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let broker = EnvironmentBroker::new(profile).unwrap();
        let response = broker.handle(
            BrokerRequest::new(["sh".into(), "-c".into(), "hostname".into()])
                .allow_host_passthrough(),
        );
        assert!(!response.ok);
        assert_eq!(response.audit.decision, BrokerDecision::Blocked);
        assert_eq!(response.audit.classification, BrokerClassification::Unknown);
        assert!(response.execution.is_none());
    }

    #[test]
    fn broker_requires_explicit_host_passthrough() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let broker = EnvironmentBroker::new(profile).unwrap();
        let response = broker.handle(BrokerRequest::new(["printf".into(), "hello".into()]));
        assert!(!response.ok);
        assert_eq!(response.audit.decision, BrokerDecision::Blocked);
        assert_eq!(
            response.audit.error.as_deref(),
            Some("host passthrough requires allow_host_passthrough=true")
        );
    }

    #[test]
    fn broker_executes_only_explicit_passthrough_and_records_host_visibility() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let broker = EnvironmentBroker::new(profile)
            .unwrap()
            .with_config(BrokerConfig::default().with_cwd("/tmp"));
        let response = broker
            .handle(BrokerRequest::new(["printf".into(), "hello".into()]).allow_host_passthrough());
        assert!(response.ok);
        assert!(!response.equivalent);
        assert_eq!(response.audit.decision, BrokerDecision::Executed);
        assert_eq!(response.audit.state, ValueState::HostVisible);
        assert_eq!(response.execution.unwrap().stdout, "hello");
    }

    #[test]
    fn jsonl_stream_returns_one_response_per_nonempty_line() {
        let profile = RuntimeProfile::from_json(PROFILE).unwrap().profile;
        let broker = EnvironmentBroker::new(profile).unwrap();
        let input = concat!(
            "{\"id\":\"one\",\"argv\":[\"hostname\"]}\n",
            "not-json\n",
            "\n",
            "{\"id\":\"two\",\"argv\":[\"bash\",\"-c\",\"pwd\"]}\n"
        );
        let mut output = Vec::new();
        let stats = broker
            .serve_jsonl(std::io::Cursor::new(input), &mut output)
            .unwrap();
        assert_eq!(
            stats,
            BrokerStats {
                requests: 3,
                responses: 3,
                malformed: 1,
            }
        );
        let responses: Vec<BrokerResponse> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 3);
        assert!(responses[0].ok);
        assert!(!responses[1].ok);
        assert!(!responses[2].ok);
    }
}
