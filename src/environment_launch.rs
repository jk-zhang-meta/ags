//! Apply an environment-contract profile to a launched agent.
//!
//! This is the observation overlay, not a guest kernel.  Changing the selected
//! AGS profile rewrites the process environment the agent inherits and installs
//! provider hooks that answer `hostname`/`uname`/`date` from the profile.  It
//! does not intercept `gethostname(2)`, `/proc`, or `os.release`.
//!
//! The agent process keeps the real `HOME`, `PATH`, and credential files so
//! Codex and Claude can start.  Carrier keys such as `WSL_*` are removed.
//! `CODEX_HOME` / `CLAUDE_CONFIG_DIR` point at a per-profile overlay that
//! symlinks the real home and writes `hooks.json` / `settings.json`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::environment::{BackendKind, EnvironmentProfile, TargetOs};
use crate::environment_runtime::{RuntimeError, RuntimeProfile};
use crate::launch::LaunchSpec;

const SELECTION_SCHEMA: &str = "ags-environment-selection-v1";
const APPLY_HEADER: &str = "ags-environment-apply-v1";

const HOOK_PY: &str = include_str!("../scripts/agent-env-hook.py");
const CONTEXT_PY: &str = include_str!("../scripts/agent-env-context.py");
const ADAPTER_PY: &str = include_str!("../scripts/agent_env_adapter.py");

const BUNDLED: &[(&str, &str)] = &[
    (
        "tokyo-macos",
        include_str!("../examples/environments/tokyo-macos.json"),
    ),
    (
        "tokyo-linux",
        include_str!("../examples/environments/tokyo-linux.json"),
    ),
    (
        "chicago-macos",
        include_str!("../examples/environments/chicago-macos.json"),
    ),
];

const CARRIER_UNSET: &[&str] = &[
    "WSL_DISTRO_NAME",
    "WSL_INTEROP",
    "WSLENV",
    "WSL2_GUI_APPS_ENABLED",
    "WSL_HOST",
    "WSL_DISTRO",
    "AGS_ENV_PROFILE",
    "AGS_BACKEND_KIND",
    "AGS_AGENT_PROVIDER",
];

/// Where AGS keeps the selected profile and generated overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayPaths {
    pub state: PathBuf,
    pub config: PathBuf,
}

impl OverlayPaths {
    pub fn from_env() -> Self {
        let state = if let Ok(dir) = std::env::var("AGENT_SESSION_STATE_DIR") {
            PathBuf::from(dir)
        } else if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
            PathBuf::from(xdg).join("ags")
        } else {
            home_dir().join(".local/state/ags")
        };
        let config = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("ags")
        } else {
            home_dir().join(".config/ags")
        };
        Self { state, config }
    }

    pub fn selection_file(&self) -> PathBuf {
        self.state.join("environment-selection.json")
    }

    pub fn environments_dir(&self) -> PathBuf {
        self.config.join("environments")
    }

    pub fn overlay_parent(&self) -> PathBuf {
        self.state.join("env-overlays")
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Persisted `ags env-use` choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSelection {
    pub schema: String,
    pub profile_path: PathBuf,
    pub profile_id: String,
    pub selected_at: String,
}

/// Carrier snapshot used to build a launch overlay without reading the
/// process environment from tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchHost {
    pub home: PathBuf,
    pub path: Option<String>,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub gemini_home: PathBuf,
    pub inherited: BTreeMap<String, String>,
}

impl LaunchHost {
    pub fn from_process() -> Self {
        let home = home_dir();
        Self {
            home: home.clone(),
            path: std::env::var("PATH").ok(),
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            claude_home: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("CLAUDE_HOME").map(PathBuf::from))
                .unwrap_or_else(|| home.join(".claude")),
            gemini_home: std::env::var_os("GEMINI_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".gemini")),
            inherited: std::env::vars().collect(),
        }
    }
}

/// Result of materialising one profile onto one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchPlan {
    pub profile_id: String,
    pub profile_path: PathBuf,
    pub overlay_root: PathBuf,
    pub provider: String,
    pub set_env: Vec<(String, String)>,
    pub unset_env: Vec<String>,
    pub extra_args: Vec<String>,
    pub virtual_observations: Vec<String>,
    pub host_visible: Vec<String>,
}

impl LaunchPlan {
    pub fn apply_to_spec(&self, mut spec: LaunchSpec) -> LaunchSpec {
        for (key, value) in &self.set_env {
            spec = spec.with_env(key, value);
        }
        for key in &self.unset_env {
            spec = spec.without_env(key);
        }
        spec
    }

    pub fn to_shell(&self) -> String {
        let mut lines = vec![format!("# {APPLY_HEADER}"), format!("# {}", self.profile_id)];
        for (key, value) in &self.set_env {
            lines.push(format!("export {key}={}", posix_single_quote(value)));
        }
        for key in &self.unset_env {
            lines.push(format!("unset {key}"));
        }
        lines.join("\n") + "\n"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyFormat {
    Shell,
    Json,
}

impl ApplyFormat {
    pub fn parse(value: &str) -> Result<Self, OverlayError> {
        match value {
            "shell" => Ok(Self::Shell),
            "json" => Ok(Self::Json),
            other => Err(OverlayError::InvalidFormat(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("invalid environment profile: {0}")]
    InvalidProfile(String),
    #[error("environment profile not found: {0}")]
    NotFound(String),
    #[error("environment overlay I/O error: {0}")]
    Io(String),
    #[error("unknown env-apply format `{0}` (use shell or json)")]
    InvalidFormat(String),
}

impl From<RuntimeError> for OverlayError {
    fn from(error: RuntimeError) -> Self {
        Self::InvalidProfile(error.to_string())
    }
}

impl From<std::io::Error> for OverlayError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileListing {
    pub id: String,
    pub source: String,
    pub path: Option<String>,
    pub selected: bool,
}

pub fn bundled_profile(name: &str) -> Option<&'static str> {
    BUNDLED
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, json)| *json)
}

pub fn load_selection(paths: &OverlayPaths) -> Result<Option<EnvironmentSelection>, OverlayError> {
    let path = paths.selection_file();
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let selection: EnvironmentSelection = serde_json::from_str(&raw)
        .map_err(|error| OverlayError::InvalidProfile(error.to_string()))?;
    if selection.schema != SELECTION_SCHEMA {
        return Err(OverlayError::InvalidProfile(format!(
            "unsupported selection schema {}",
            selection.schema
        )));
    }
    Ok(Some(selection))
}

pub fn save_selection(
    paths: &OverlayPaths,
    spec: &str,
) -> Result<EnvironmentSelection, OverlayError> {
    let profile_path = resolve_spec_to_path(paths, spec)?;
    let raw = fs::read_to_string(&profile_path)?;
    let runtime = RuntimeProfile::from_json(&raw)?;
    fs::create_dir_all(&paths.state)?;
    let selection = EnvironmentSelection {
        schema: SELECTION_SCHEMA.to_string(),
        profile_path,
        profile_id: runtime.profile.id.clone(),
        selected_at: chrono::Utc::now().to_rfc3339(),
    };
    let encoded = serde_json::to_string_pretty(&selection)
        .map_err(|error| OverlayError::Io(error.to_string()))?;
    atomic_write(&paths.selection_file(), encoded.as_bytes())?;
    Ok(selection)
}

pub fn clear_selection(paths: &OverlayPaths) -> Result<bool, OverlayError> {
    let path = paths.selection_file();
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path)?;
    Ok(true)
}

pub fn list_profiles(paths: &OverlayPaths) -> Result<Vec<ProfileListing>, OverlayError> {
    let selected = load_selection(paths)?;
    let selected_path = selected.as_ref().map(|row| row.profile_path.clone());
    let mut rows = Vec::new();
    if let Some(selection) = &selected {
        rows.push(ProfileListing {
            id: selection.profile_id.clone(),
            source: "selected".into(),
            path: Some(selection.profile_path.display().to_string()),
            selected: true,
        });
    }
    if let Ok(entries) = fs::read_dir(paths.environments_dir()) {
        let mut files: Vec<_> = entries.filter_map(Result::ok).collect();
        files.sort_by_key(|entry| entry.file_name());
        for entry in files {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if selected_path.as_ref() == Some(&path) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("profile")
                .to_string();
            rows.push(ProfileListing {
                id,
                source: "config".into(),
                path: Some(path.display().to_string()),
                selected: false,
            });
        }
    }
    for (id, _) in BUNDLED {
        rows.push(ProfileListing {
            id: (*id).to_string(),
            source: "bundled".into(),
            path: None,
            selected: false,
        });
    }
    Ok(rows)
}

pub fn resolve_profile(
    paths: &OverlayPaths,
    explicit: Option<&Path>,
) -> Result<Option<(PathBuf, RuntimeProfile)>, OverlayError> {
    let path = if let Some(explicit) = explicit {
        Some(explicit.to_path_buf())
    } else if let Ok(value) = std::env::var("AGS_ENV_PROFILE") {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    } else {
        load_selection(paths)?.map(|row| row.profile_path)
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&path).map_err(|_| {
        OverlayError::NotFound(path.display().to_string())
    })?;
    Ok(Some((path, RuntimeProfile::from_json(&raw)?)))
}

pub fn materialize(
    paths: &OverlayPaths,
    profile_path: &Path,
    profile: &EnvironmentProfile,
    provider: &str,
    host: &LaunchHost,
) -> Result<LaunchPlan, OverlayError> {
    let kind = ProviderKind::parse(provider);
    let overlay_root = paths
        .overlay_parent()
        .join(sanitize_id(&profile.id))
        .join(kind.dir_name());
    write_overlay(profile_path, profile, kind, host, &overlay_root)?;

    let mut set_env = Vec::new();
    set_env.push(("TZ".into(), profile.clock.timezone.clone()));
    set_env.push(("LANG".into(), profile.locale.lang.clone()));
    set_env.push(("LC_ALL".into(), profile.locale.lc_all.clone()));
    set_env.push((
        "LC_TIME".into(),
        profile
            .locale
            .lc_time
            .clone()
            .unwrap_or_else(|| profile.locale.lc_all.clone()),
    ));
    set_env.push(("USER".into(), profile.identity.username.clone()));
    set_env.push(("LOGNAME".into(), profile.identity.username.clone()));
    set_env.push(("HOSTNAME".into(), profile.identity.hostname.clone()));
    match kind {
        ProviderKind::Codex => {
            set_env.push((
                "CODEX_HOME".into(),
                overlay_root.join("home").display().to_string(),
            ));
        }
        ProviderKind::Claude => {
            set_env.push((
                "CLAUDE_CONFIG_DIR".into(),
                overlay_root.join("home").display().to_string(),
            ));
        }
        ProviderKind::Gemini => {
            set_env.push((
                "GEMINI_HOME".into(),
                overlay_root.join("home").display().to_string(),
            ));
        }
        ProviderKind::Generic => {}
    }

    let mut unset_env: Vec<String> = CARRIER_UNSET.iter().map(|key| (*key).to_string()).collect();
    for (key, _) in &host.inherited {
        if is_carrier_key(key) && !unset_env.iter().any(|existing| existing == key) {
            unset_env.push(key.clone());
        }
    }
    unset_env.sort();
    unset_env.dedup();

    Ok(LaunchPlan {
        profile_id: profile.id.clone(),
        profile_path: profile_path.to_path_buf(),
        overlay_root,
        provider: kind.dir_name().to_string(),
        set_env,
        unset_env,
        extra_args: Vec::new(),
        virtual_observations: vec![
            "USER/LOGNAME/HOSTNAME/TZ/LANG/LC_* process environment".into(),
            "hostname/whoami/id/uname/pwd/date/env/locale via provider hooks".into(),
            "SessionStart additionalContext (Claude)".into(),
        ],
        host_visible: host_visible_notes(profile, host),
    })
}

pub fn apply_to_spec(
    spec: LaunchSpec,
    paths: &OverlayPaths,
    explicit_profile: Option<&Path>,
    provider: &str,
    host: &LaunchHost,
) -> Result<LaunchSpec, OverlayError> {
    let Some((profile_path, runtime)) = resolve_profile(paths, explicit_profile)? else {
        return Ok(spec);
    };
    let plan = materialize(paths, &profile_path, &runtime.profile, provider, host)?;
    Ok(plan.apply_to_spec(spec))
}

pub fn render_apply(plan: Option<&LaunchPlan>, format: ApplyFormat) -> Result<String, OverlayError> {
    match (plan, format) {
        (None, ApplyFormat::Shell) => Ok(format!("# {APPLY_HEADER}\n# none\n")),
        (None, ApplyFormat::Json) => Ok("{\"profile_id\":null}\n".into()),
        (Some(plan), ApplyFormat::Shell) => Ok(plan.to_shell()),
        (Some(plan), ApplyFormat::Json) => Ok(serde_json::to_string_pretty(plan)
            .map_err(|error| OverlayError::Io(error.to_string()))?
            + "\n"),
    }
}

fn resolve_spec_to_path(paths: &OverlayPaths, spec: &str) -> Result<PathBuf, OverlayError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(OverlayError::NotFound(spec.to_string()));
    }
    let candidate = PathBuf::from(trimmed);
    if candidate.is_file() {
        return candidate.canonicalize().map_err(|error| {
            OverlayError::Io(format!("{}: {error}", candidate.display()))
        });
    }
    let config_candidate = paths.environments_dir().join(format!("{trimmed}.json"));
    if config_candidate.is_file() {
        return config_candidate.canonicalize().map_err(|error| {
            OverlayError::Io(format!("{}: {error}", config_candidate.display()))
        });
    }
    if let Some(json) = bundled_profile(trimmed) {
        let dest_dir = paths.environments_dir();
        fs::create_dir_all(&dest_dir)?;
        let dest = dest_dir.join(format!("{trimmed}.json"));
        atomic_write(&dest, json.as_bytes())?;
        return dest.canonicalize().map_err(|error| {
            OverlayError::Io(format!("{}: {error}", dest.display()))
        });
    }
    Err(OverlayError::NotFound(spec.to_string()))
}

fn write_overlay(
    profile_path: &Path,
    profile: &EnvironmentProfile,
    kind: ProviderKind,
    host: &LaunchHost,
    overlay_root: &Path,
) -> Result<(), OverlayError> {
    let staging = overlay_root.with_extension("staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let scripts = staging.join("scripts");
    fs::create_dir_all(&scripts)?;
    atomic_write(&scripts.join("agent-env-hook.py"), HOOK_PY.as_bytes())?;
    atomic_write(&scripts.join("agent-env-context.py"), CONTEXT_PY.as_bytes())?;
    atomic_write(&scripts.join("agent_env_adapter.py"), ADAPTER_PY.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["agent-env-hook.py", "agent-env-context.py", "agent_env_adapter.py"] {
            let path = scripts.join(name);
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)?;
        }
    }
    let profile_dest = staging.join("profile.json");
    let encoded = if profile_path.is_file() {
        fs::read_to_string(profile_path)?
    } else {
        serde_json::to_string_pretty(profile)
            .map_err(|error| OverlayError::Io(error.to_string()))?
    };
    atomic_write(&profile_dest, encoded.as_bytes())?;

    let provider_home = staging.join("home");
    // Hook command lines must name the post-rename paths.  Staging is swapped
    // onto `overlay_root` after these files are written.
    let final_scripts = overlay_root.join("scripts");
    let final_profile = overlay_root.join("profile.json");
    match kind {
        ProviderKind::Codex => {
            symlink_home(&host.codex_home, &provider_home, &["hooks.json"])?;
            atomic_write(
                &provider_home.join("hooks.json"),
                hook_config_json("codex", &final_scripts, &final_profile).as_bytes(),
            )?;
        }
        ProviderKind::Claude => {
            symlink_home(
                &host.claude_home,
                &provider_home,
                &["settings.json"],
            )?;
            let merged = merge_claude_settings(
                &host.claude_home.join("settings.json"),
                &final_scripts,
                &final_profile,
            )?;
            atomic_write(&provider_home.join("settings.json"), merged.as_bytes())?;
        }
        ProviderKind::Gemini => {
            symlink_home(&host.gemini_home, &provider_home, &["settings.json"])?;
            atomic_write(
                &provider_home.join("settings.json"),
                gemini_settings_json(&final_scripts, &final_profile).as_bytes(),
            )?;
        }
        ProviderKind::Generic => {}
    }

    if overlay_root.exists() {
        fs::remove_dir_all(overlay_root)?;
    }
    if let Some(parent) = overlay_root.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&staging, overlay_root)?;
    Ok(())
}

fn symlink_home(real: &Path, overlay: &Path, skip: &[&str]) -> Result<(), OverlayError> {
    fs::create_dir_all(overlay)?;
    if !real.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(real)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|skipped| name == *skipped) {
            continue;
        }
        let dest = overlay.join(&name);
        #[cfg(unix)]
        {
            if let Err(error) = std::os::unix::fs::symlink(entry.path(), &dest)
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(error.into());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = dest;
        }
    }
    Ok(())
}

fn hook_command(scripts: &Path, provider: &str, profile: &Path, context: bool) -> String {
    let script = if context {
        scripts.join("agent-env-context.py")
    } else {
        scripts.join("agent-env-hook.py")
    };
    format!(
        "python3 {} --provider {provider} --profile {}",
        script.display(),
        profile.display()
    )
}

fn hook_config_json(provider: &str, scripts: &Path, profile: &Path) -> String {
    let command = hook_command(scripts, provider, profile, false);
    let timeout = if provider == "codex" { 5 } else { 5000 };
    let mut hooks = serde_json::json!({
        "PreToolUse": [hook_entry(&command, timeout, if provider == "codex" { "^Bash$" } else { "Bash" })],
        "PermissionRequest": [hook_entry(&command, timeout, if provider == "codex" { "^Bash$" } else { "Bash" })],
        "PostToolUse": [hook_entry(&command, timeout, if provider == "codex" { "^Bash$" } else { "Bash" })],
    });
    if provider != "codex" {
        let context = hook_command(scripts, provider, profile, true);
        hooks["SessionStart"] = serde_json::json!([hook_entry(
            &context,
            timeout,
            "startup|resume|clear"
        )]);
    }
    serde_json::json!({ "hooks": hooks }).to_string()
}

fn hook_entry(command: &str, timeout: u64, matcher: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": matcher,
        "hooks": [{ "type": "command", "command": command, "timeout": timeout }]
    })
}

fn merge_claude_settings(
    existing: &Path,
    scripts: &Path,
    profile: &Path,
) -> Result<String, OverlayError> {
    let mut root = if existing.is_file() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(existing)?)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let overlay: serde_json::Value = serde_json::from_str(&hook_config_json("claude", scripts, profile))
        .map_err(|error| OverlayError::Io(error.to_string()))?;
    root["hooks"] = overlay["hooks"].clone();
    Ok(root.to_string())
}

fn gemini_settings_json(scripts: &Path, profile: &Path) -> String {
    let command = hook_command(scripts, "gemini", profile, false);
    serde_json::json!({
        "hooks": {
            "BeforeTool": [{
                "matcher": "run_shell_command|shell|execute_command|Bash",
                "sequential": true,
                "hooks": [{ "name": "ags-environment-gate", "type": "command", "command": command, "timeout": 5000 }]
            }],
            "AfterTool": [{
                "matcher": "run_shell_command|shell|execute_command|Bash",
                "sequential": true,
                "hooks": [{ "name": "ags-environment-audit", "type": "command", "command": command, "timeout": 5000 }]
            }]
        }
    })
    .to_string()
}

fn host_visible_notes(profile: &EnvironmentProfile, host: &LaunchHost) -> Vec<String> {
    let mut notes = vec![
        "gethostname(2), uid/gid syscalls, and /proc remain the carrier".into(),
        "real HOME/PATH/cwd are kept so the agent can start and find sessions".into(),
        "hook script paths in overlay config can reveal the AGS state directory".into(),
    ];
    let host_os = if cfg!(target_os = "macos") {
        TargetOs::Macos
    } else {
        TargetOs::Linux
    };
    if profile.target.os != host_os {
        notes.push(format!(
            "profile OS is {:?} but this process is {:?}; os.release/uname syscall stay host-visible",
            profile.target.os, host_os
        ));
    }
    if host.inherited.contains_key("WSL_DISTRO_NAME") || host.inherited.contains_key("WSL_INTEROP")
    {
        notes.push("WSL env keys are unset; /proc/version and /mnt/c still identify WSL".into());
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.to_string_lossy();
        if cwd.contains("/mnt/c") || cwd.contains("/mnt/wsl") {
            notes.push("current workspace path reveals a Windows/WSL mount".into());
        }
    }
    if profile.backend.kind == BackendKind::LinuxVps {
        notes.push("VPS provenance is not virtualised by this overlay".into());
    }
    notes
}

fn is_carrier_key(key: &str) -> bool {
    key.starts_with("WSL")
        || key == "WT_SESSION"
        || key == "WSLENV"
        || key.starts_with("AGS_ENV_")
        || key == "AGS_BACKEND_KIND"
}

fn sanitize_id(id: &str) -> String {
    let mut out = String::new();
    for character in id.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            out.push(character);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "profile".into()
    } else {
        out
    }
}

fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), OverlayError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Codex,
    Claude,
    Gemini,
    Generic,
}

impl ProviderKind {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "cod" => Self::Codex,
            "claude" | "claude-code" | "cc" => Self::Claude,
            "gemini" | "gmi" => Self::Gemini,
            _ => Self::Generic,
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Generic => "generic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tokyo() -> RuntimeProfile {
        RuntimeProfile::from_json(bundled_profile("tokyo-macos").unwrap()).unwrap()
    }

    fn paths_in(root: &Path) -> OverlayPaths {
        OverlayPaths {
            state: root.join("state"),
            config: root.join("config"),
        }
    }

    fn host_in(root: &Path) -> LaunchHost {
        let home = root.join("home");
        fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        fs::write(home.join(".codex/auth.json"), "{}").unwrap();
        fs::write(home.join(".codex/hooks.json"), "{\"hooks\":{}}").unwrap();
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"theme":"dark","hooks":{}}"#,
        )
        .unwrap();
        LaunchHost {
            home: home.clone(),
            path: Some("/usr/bin:/bin".into()),
            codex_home: home.join(".codex"),
            claude_home: home.join(".claude"),
            gemini_home: home.join(".gemini"),
            inherited: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), home.display().to_string()),
                ("WSL_DISTRO_NAME".into(), "Ubuntu".into()),
                ("AGS_ENV_PROFILE".into(), "/tmp/secret.json".into()),
            ]),
        }
    }

    #[test]
    fn env_use_accepts_bundled_name_and_show_round_trips() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let selection = save_selection(&paths, "tokyo-macos").unwrap();
        assert_eq!(selection.profile_id, "tokyo-macos");
        assert!(selection.profile_path.is_file());
        let loaded = load_selection(&paths).unwrap().unwrap();
        assert_eq!(loaded.profile_id, "tokyo-macos");
        assert!(clear_selection(&paths).unwrap());
        assert!(load_selection(&paths).unwrap().is_none());
    }

    #[test]
    fn launch_env_keeps_path_and_home_and_drops_wsl() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let host = host_in(tmp.path());
        let profile = tokyo();
        let plan = materialize(
            &paths,
            Path::new("tokyo-macos"),
            &profile.profile,
            "codex",
            &host,
        )
        .unwrap();
        let keys: Vec<_> = plan.set_env.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"TZ"));
        assert!(keys.contains(&"USER"));
        assert!(keys.contains(&"HOSTNAME"));
        assert!(keys.contains(&"CODEX_HOME"));
        assert!(!keys.contains(&"PATH"));
        assert!(!keys.contains(&"HOME"));
        assert_eq!(
            plan.set_env
                .iter()
                .find(|(key, _)| key == "TZ")
                .map(|(_, value)| value.as_str()),
            Some("Asia/Tokyo")
        );
        assert_eq!(
            plan.set_env
                .iter()
                .find(|(key, _)| key == "HOSTNAME")
                .map(|(_, value)| value.as_str()),
            Some("tokyo-node")
        );
        assert!(plan.unset_env.iter().any(|key| key == "WSL_DISTRO_NAME"));
        assert!(plan.unset_env.iter().any(|key| key == "AGS_ENV_PROFILE"));
    }

    #[test]
    fn overlay_writes_hooks_and_symlinks_real_auth_without_onedrive_path() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let host = host_in(tmp.path());
        let profile_path = tmp.path().join("tokyo-macos.json");
        fs::write(&profile_path, bundled_profile("tokyo-macos").unwrap()).unwrap();
        let profile = tokyo();
        let plan = materialize(&paths, &profile_path, &profile.profile, "codex", &host).unwrap();
        let hooks = fs::read_to_string(plan.overlay_root.join("home/hooks.json")).unwrap();
        assert!(hooks.contains("agent-env-hook.py"));
        assert!(hooks.contains("--provider codex"));
        assert!(!hooks.contains("/mnt/c"));
        assert!(!hooks.contains("OneDrive"));
        let auth = plan.overlay_root.join("home/auth.json");
        assert!(auth.is_symlink() || auth.is_file());
        let spec = LaunchSpec::new("codex", ["resume".into(), "abc".into()]);
        let spec = plan.apply_to_spec(spec);
        assert!(spec.env.iter().any(|(key, _)| key == "TZ"));
        assert!(spec.env_removals().iter().any(|key| key == "WSL_DISTRO_NAME"));
    }

    #[test]
    fn claude_overlay_merges_settings_and_keeps_theme() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let host = host_in(tmp.path());
        let profile_path = tmp.path().join("tokyo-macos.json");
        fs::write(&profile_path, bundled_profile("tokyo-macos").unwrap()).unwrap();
        let profile = tokyo();
        let plan = materialize(&paths, &profile_path, &profile.profile, "claude", &host).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(plan.overlay_root.join("home/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["theme"], "dark");
        assert!(settings["hooks"]["PreToolUse"].is_array());
        assert!(settings["hooks"]["SessionStart"].is_array());
        assert!(
            plan.set_env
                .iter()
                .any(|(key, _)| key == "CLAUDE_CONFIG_DIR")
        );
    }

    #[test]
    fn apply_to_spec_is_noop_without_selection() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let host = host_in(tmp.path());
        let spec = LaunchSpec::new("codex", ["resume".into(), "abc".into()]);
        let applied = apply_to_spec(spec.clone(), &paths, None, "codex", &host).unwrap();
        assert_eq!(applied.env, spec.env);
    }

    #[test]
    fn shell_apply_quotes_values() {
        let plan = LaunchPlan {
            profile_id: "tokyo-macos".into(),
            profile_path: PathBuf::from("/tmp/p.json"),
            overlay_root: PathBuf::from("/tmp/o"),
            provider: "codex".into(),
            set_env: vec![("TZ".into(), "Asia/Tokyo".into()), ("USER".into(), "o'neil".into())],
            unset_env: vec!["WSL_DISTRO_NAME".into()],
            extra_args: Vec::new(),
            virtual_observations: Vec::new(),
            host_visible: Vec::new(),
        };
        let shell = plan.to_shell();
        assert!(shell.contains("export TZ='Asia/Tokyo'"));
        assert!(shell.contains("export USER='o'\\''neil'"));
        assert!(shell.contains("unset WSL_DISTRO_NAME"));
    }

    #[test]
    fn linux_profile_on_this_host_records_os_mismatch_when_needed() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let host = host_in(tmp.path());
        let profile = RuntimeProfile::from_json(bundled_profile("tokyo-linux").unwrap()).unwrap();
        let plan = materialize(
            &paths,
            Path::new("tokyo-linux"),
            &profile.profile,
            "codex",
            &host,
        )
        .unwrap();
        let joined = plan.host_visible.join(" ");
        if cfg!(target_os = "macos") {
            assert!(joined.contains("os.release"));
        }
        assert!(joined.contains("real HOME/PATH/cwd"));
    }
}
