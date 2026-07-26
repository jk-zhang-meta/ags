//! Typed JSON response structs for all machine-readable CLI outputs.
//!
//! Every `--json` subcommand serializes one of these structs. Using concrete
//! `#[derive(Serialize)]` types instead of ad-hoc `serde_json::json!` objects
//! guarantees that field names, types, and `schema_version` are consistent
//! across the codebase and testable at compile time.

use std::path::PathBuf;

use serde::Serialize;

use crate::ir::Fidelity;
use crate::store::{Availability, OriginState};

/// Current schema version for all JSON envelopes and per-record outputs.
///
/// Bump this when adding/removing/renaming fields in any response struct.
pub const SCHEMA_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// `list --json`
// ---------------------------------------------------------------------------

/// Versioned envelope wrapping `list --json` output.
#[derive(Debug, Clone, Serialize)]
pub struct ListEnvelope {
    pub schema_version: u32,
    pub items: Vec<ListItem>,
}

impl ListEnvelope {
    pub fn new(items: Vec<ListItem>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            items,
        }
    }
}

/// A single session entry in `list --json` output.
#[derive(Debug, Clone, Serialize)]
pub struct ListItem {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: String,
    pub title: Option<String>,
    /// Provider-native session name (e.g. Claude Code `/rename` title, Amp
    /// thread title). `null` for providers without such a concept.
    pub native_name: Option<String>,
    pub messages: usize,
    pub workspace: Option<String>,
    pub started_at: Option<i64>,
    pub last_active_at: Option<i64>,
    pub file_size_bytes: u64,
    pub file_size_kb: u64,
    pub unique_user_messages: usize,
    pub avg_agent_response_chars: f64,
    pub avg_agent_response_chars_rounded: u64,
    pub tool_uses: usize,
    pub path: String,
    /// Workspace name derived from session metadata (directory basename or title).
    pub workspace_name: Option<String>,
    /// How `workspace_name` was determined.
    pub workspace_name_source: Option<String>,
    /// Repository name from filesystem git root (only when `--enrich-fs` is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
}

// ---------------------------------------------------------------------------
// `info --json`
// ---------------------------------------------------------------------------

/// Response struct for `info --json`.
#[derive(Debug, Clone, Serialize)]
pub struct InfoResponse {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: String,
    pub title: Option<String>,
    /// Provider-native session name (e.g. Claude Code `/rename` title, Amp
    /// thread title). `null` for providers without such a concept.
    pub native_name: Option<String>,
    pub workspace: Option<String>,
    pub messages: usize,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub model_name: Option<String>,
    pub source_path: String,
    pub metadata: serde_json::Value,
    /// Workspace name derived from session metadata (directory basename or title).
    pub workspace_name: Option<String>,
    /// How `workspace_name` was determined.
    pub workspace_name_source: Option<String>,
    /// Repository name from filesystem git root (only when `--enrich-fs` is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    /// Tail of the transcript (last few turns), present only with `--peek`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_tail: Option<Vec<crate::model::TranscriptTurn>>,
}

// ---------------------------------------------------------------------------
// `providers --json`
// ---------------------------------------------------------------------------

/// A single provider entry in `providers --json`.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub slug: String,
    pub alias: String,
    pub installed: bool,
    pub version: Option<String>,
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// `resume --json` (success)
// ---------------------------------------------------------------------------

/// Response struct for a successful `resume --json` (including dry-run).
#[derive(Debug, Clone, Serialize)]
pub struct ResumeSuccess {
    pub ok: bool,
    pub source_provider: String,
    pub target_provider: String,
    pub source_session_id: String,
    pub target_session_id: Option<String>,
    pub written_paths: Option<Vec<String>>,
    pub resume_command: Option<String>,
    pub dry_run: bool,
    /// How much of the session survived the conversion, as a snake_case grade
    /// (`"conversation_only"`, `"history_incomplete"`, …).
    ///
    /// The one field a script needs to decide whether the converted session is
    /// safe to resume unattended.
    pub fidelity: Fidelity,
    /// The command `--launch` / `--launch-dry-run` resolved to, shell-quoted.
    ///
    /// `null` when no launch was asked for. Present under `--json` so the
    /// launcher no longer has to route its output to stderr to keep stdout
    /// parseable.
    pub launch_command: Option<String>,
    /// Whether `launch_command` actually names the converted session.
    ///
    /// `false` for the providers that have no session-id resume form: the file
    /// was written correctly, the agent simply will not be pointed at it.
    /// `null` when no launch was asked for, which is not the same as `false`.
    pub launch_targets_session: Option<bool>,
    pub warnings: Vec<String>,
    /// The session store read a session other than the one that was named.
    ///
    /// Omitted entirely — not `null` — whenever the conversion read what it was
    /// asked to, which includes every `--no-store` run. That is deliberate: a
    /// script that worked before the store exists sees byte-identical JSON, and
    /// the field's mere presence is the signal that the source was substituted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_selection: Option<SourceSelectionJson>,
}

/// Which incarnation the store read instead, and what the named one would have
/// cost.
///
/// The human form is one sentence from `SourceChoice::explain`. This is the same
/// information as fields, because a caller that has to `grep` a sentence for
/// "would have cost 3 capsules" has no contract at all.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSelectionJson {
    /// Our identifier for the conversation — the one `resume <record-id>` takes.
    pub record_id: String,
    /// Provider slug of the session that was actually read.
    pub provider: String,
    /// That session's provider-native id.
    pub session_id: String,
    /// `"origin"` or `"derived"`.
    pub role: String,
    /// `"ready"` when the session's own file was read, `"archived"` when the
    /// live origin was gone and the store's byte copy stood in.
    pub availability: String,
    /// How the origin reference resolved: `"unchanged"`, `"unchanged_verified"`,
    /// `"grew"`, or `"unavailable"`. `null` for a derived incarnation, which the
    /// store never snapshotted.
    ///
    /// All four are reported. `"unchanged"` without `_verified` is the cheap
    /// answer — same size and mtime, bytes not re-read — and says so rather than
    /// claiming a verification it did not run.
    pub origin_state: Option<String>,
    /// Why that resolution, in the store's own words. `null` when unchanged.
    pub origin_detail: Option<String>,
    /// Capsules in the chosen source that the target's vendor can replay.
    pub capsules: usize,
    /// Bytes of sealed material behind `capsules`.
    pub capsule_bytes: usize,
    /// Provider slug of the session the user named.
    pub named_provider: String,
    /// Provider-native id of the session the user named.
    pub named_session_id: String,
    /// Capsules that reading the named session instead would have cost.
    pub cost_capsules: usize,
    /// Bytes of sealed material behind `cost_capsules`.
    pub cost_capsule_bytes: usize,
}

impl SourceSelectionJson {
    /// The structured form of a selection, or `None` when the store read exactly
    /// what it was asked to and there is nothing to report.
    pub fn of(selection: &crate::pipeline::SourceSelection) -> Option<Self> {
        if !selection.overrode() {
            return None;
        }
        let chosen = selection.chosen()?;
        let named = selection.choice.find(&selection.named);
        let (origin_state, origin_detail) = match &chosen.origin_state {
            None => (None, None),
            Some(state) => match state {
                OriginState::Unchanged { rehashed: false } => (Some("unchanged"), None),
                OriginState::Unchanged { rehashed: true } => (Some("unchanged_verified"), None),
                OriginState::Grew { .. } => (Some("grew"), Some(state.describe())),
                OriginState::Unavailable { .. } => (Some("unavailable"), Some(state.describe())),
            },
        };
        Some(Self {
            record_id: selection.record_id.clone(),
            provider: chosen.key.provider.clone(),
            session_id: chosen.key.provider_session_id.clone(),
            role: chosen.label().to_string(),
            availability: match &chosen.availability {
                Availability::Ready => "ready",
                Availability::Archived => "archived",
                // Not reachable through `chosen()`, which filters on
                // readability; spelled out so a new variant is a compile error.
                Availability::Unavailable { .. } => "unavailable",
            }
            .to_string(),
            origin_state: origin_state.map(str::to_string),
            origin_detail,
            capsules: chosen.capsules.fitting(),
            capsule_bytes: chosen.capsules.fitting_bytes(),
            named_provider: selection.named.provider.clone(),
            named_session_id: selection.named.provider_session_id.clone(),
            cost_capsules: chosen
                .capsules
                .fitting()
                .saturating_sub(named.map_or(0, |named| named.capsules.fitting())),
            cost_capsule_bytes: chosen
                .capsules
                .fitting_bytes()
                .saturating_sub(named.map_or(0, |named| named.capsules.fitting_bytes())),
        })
    }
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// JSON envelope for error responses.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub ok: bool,
    pub error_type: String,
    pub message: String,
}

impl ErrorEnvelope {
    pub fn new(error_type: &str, message: String) -> Self {
        Self {
            ok: false,
            error_type: error_type.to_string(),
            message,
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace name derivation
// ---------------------------------------------------------------------------

/// Source description for how `workspace_name` was resolved.
pub const WS_NAME_SOURCE_SESSION_PATH: &str = "session_workspace_path";
pub const WS_NAME_SOURCE_NONE: &str = "none";

/// Derive a human-readable workspace name from a workspace path.
///
/// Returns the last component of the path (the directory name) as the name,
/// along with the source tag describing how it was derived.
pub fn workspace_name_from_path(workspace: Option<&PathBuf>) -> (Option<String>, Option<String>) {
    match workspace {
        Some(ws) => {
            let name = ws
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            if name.is_some() {
                (name, Some(WS_NAME_SOURCE_SESSION_PATH.to_string()))
            } else {
                (None, Some(WS_NAME_SOURCE_NONE.to_string()))
            }
        }
        None => (None, Some(WS_NAME_SOURCE_NONE.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Schema version is consistent
    // -----------------------------------------------------------------------

    #[test]
    fn schema_version_is_2() {
        assert_eq!(SCHEMA_VERSION, 2);
    }

    // -----------------------------------------------------------------------
    // ListEnvelope serialization
    // -----------------------------------------------------------------------

    #[test]
    fn list_envelope_empty_items_serializes() {
        let envelope = ListEnvelope::new(vec![]);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert!(json["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn list_envelope_with_items_serializes() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid-1".to_string(),
            provider: "claude-code".to_string(),
            title: Some("Test".to_string()),
            native_name: Some("Renamed Session".to_string()),
            messages: 10,
            workspace: Some("/data/projects/test".to_string()),
            started_at: Some(1_700_000_000_000),
            last_active_at: Some(1_700_001_000_000),
            file_size_bytes: 4096,
            file_size_kb: 4,
            unique_user_messages: 3,
            avg_agent_response_chars: 500.5,
            avg_agent_response_chars_rounded: 501,
            tool_uses: 7,
            path: "/tmp/session.jsonl".to_string(),
            workspace_name: Some("test".to_string()),
            workspace_name_source: Some("session_workspace_path".to_string()),
            repo_name: None,
        };
        let envelope = ListEnvelope::new(vec![item]);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        let first = &json["items"][0];
        assert_eq!(first["schema_version"], 2);
        assert_eq!(first["session_id"], "sid-1");
        assert_eq!(first["provider"], "claude-code");
        assert_eq!(first["native_name"], "Renamed Session");
        assert_eq!(first["messages"], 10);
        assert_eq!(first["workspace_name"], "test");
        assert_eq!(first["workspace_name_source"], "session_workspace_path");
    }

    // -----------------------------------------------------------------------
    // InfoResponse serialization
    // -----------------------------------------------------------------------

    #[test]
    fn info_response_serializes_all_fields() {
        let info = InfoResponse {
            schema_version: SCHEMA_VERSION,
            session_id: "sid-info".to_string(),
            provider: "codex".to_string(),
            title: None,
            native_name: None,
            workspace: None,
            messages: 5,
            started_at: None,
            ended_at: None,
            model_name: Some("gpt-4".to_string()),
            source_path: "/tmp/session.jsonl".to_string(),
            metadata: serde_json::json!({"key": "value"}),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
            transcript_tail: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["session_id"], "sid-info");
        assert_eq!(json["provider"], "codex");
        assert!(json["title"].is_null());
        assert!(json["native_name"].is_null());
        assert!(
            !json.as_object().unwrap().contains_key("transcript_tail"),
            "transcript_tail must be omitted when not peeking"
        );
        assert!(json["workspace"].is_null());
        assert_eq!(json["messages"], 5);
        assert_eq!(json["model_name"], "gpt-4");
        assert!(json["workspace_name"].is_null());
        assert_eq!(json["workspace_name_source"], "none");
    }

    // -----------------------------------------------------------------------
    // ProviderInfo serialization
    // -----------------------------------------------------------------------

    #[test]
    fn provider_info_serializes() {
        let pi = ProviderInfo {
            name: "Claude Code".to_string(),
            slug: "claude-code".to_string(),
            alias: "cc".to_string(),
            installed: true,
            version: Some("1.0".to_string()),
            evidence: vec!["found binary".to_string()],
        };
        let json = serde_json::to_value(&pi).unwrap();
        assert_eq!(json["name"], "Claude Code");
        assert_eq!(json["slug"], "claude-code");
        assert_eq!(json["alias"], "cc");
        assert_eq!(json["installed"], true);
        assert_eq!(json["version"], "1.0");
        assert_eq!(json["evidence"][0], "found binary");
    }

    // -----------------------------------------------------------------------
    // ResumeSuccess serialization
    // -----------------------------------------------------------------------

    #[test]
    fn resume_success_dry_run_serializes() {
        let rs = ResumeSuccess {
            ok: true,
            source_provider: "claude-code".to_string(),
            target_provider: "codex".to_string(),
            source_session_id: "sid-src".to_string(),
            target_session_id: None,
            written_paths: None,
            resume_command: None,
            dry_run: true,
            fidelity: Fidelity::ConversationOnly,
            launch_command: None,
            launch_targets_session: None,
            warnings: vec![],
            source_selection: None,
        };
        let json = serde_json::to_value(&rs).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["dry_run"], true);
        assert!(json["target_session_id"].is_null());
        assert!(json["written_paths"].is_null());
        assert!(json["resume_command"].is_null());
        assert_eq!(json["fidelity"], "conversation_only");
        // Not `false`: no launch was asked for, so there is nothing to target.
        assert!(json["launch_command"].is_null());
        assert!(json["launch_targets_session"].is_null());
    }

    #[test]
    fn resume_success_actual_write_serializes() {
        let rs = ResumeSuccess {
            ok: true,
            source_provider: "codex".to_string(),
            target_provider: "claude-code".to_string(),
            source_session_id: "sid-src".to_string(),
            target_session_id: Some("sid-tgt".to_string()),
            written_paths: Some(vec!["/tmp/written.jsonl".to_string()]),
            resume_command: Some("claude --resume sid-tgt".to_string()),
            dry_run: false,
            fidelity: Fidelity::HistoryIncomplete,
            launch_command: Some("claude --resume sid-tgt".to_string()),
            launch_targets_session: Some(true),
            warnings: vec!["missing workspace".to_string()],
            source_selection: None,
        };
        let json = serde_json::to_value(&rs).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["dry_run"], false);
        assert_eq!(json["target_session_id"], "sid-tgt");
        assert_eq!(json["written_paths"][0], "/tmp/written.jsonl");
        assert_eq!(json["resume_command"], "claude --resume sid-tgt");
        assert_eq!(json["warnings"][0], "missing workspace");
        assert_eq!(json["fidelity"], "history_incomplete");
        assert_eq!(json["launch_command"], "claude --resume sid-tgt");
        assert_eq!(json["launch_targets_session"], true);
    }

    // -----------------------------------------------------------------------
    // ErrorEnvelope serialization
    // -----------------------------------------------------------------------

    #[test]
    fn list_item_repo_name_omitted_when_none() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            title: None,
            native_name: None,
            messages: 0,
            workspace: None,
            started_at: None,
            last_active_at: None,
            file_size_bytes: 0,
            file_size_kb: 0,
            unique_user_messages: 0,
            avg_agent_response_chars: 0.0,
            avg_agent_response_chars_rounded: 0,
            tool_uses: 0,
            path: "/tmp/x".to_string(),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("repo_name"),
            "repo_name should be omitted from JSON when None"
        );
        assert!(
            json.as_object().unwrap().contains_key("native_name"),
            "native_name is always present (null when absent)"
        );
    }

    #[test]
    fn list_item_repo_name_present_when_set() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            title: None,
            native_name: None,
            messages: 0,
            workspace: Some("/data/projects/my_repo".to_string()),
            started_at: None,
            last_active_at: None,
            file_size_bytes: 0,
            file_size_kb: 0,
            unique_user_messages: 0,
            avg_agent_response_chars: 0.0,
            avg_agent_response_chars_rounded: 0,
            tool_uses: 0,
            path: "/tmp/x".to_string(),
            workspace_name: Some("my_repo".to_string()),
            workspace_name_source: Some("session_workspace_path".to_string()),
            repo_name: Some("my_repo".to_string()),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["repo_name"], "my_repo");
    }

    #[test]
    fn info_response_repo_name_omitted_when_none() {
        let info = InfoResponse {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            title: None,
            native_name: None,
            workspace: None,
            messages: 0,
            started_at: None,
            ended_at: None,
            model_name: None,
            source_path: "/tmp/x".to_string(),
            metadata: serde_json::json!(null),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
            transcript_tail: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("repo_name"),
            "repo_name should be omitted from info JSON when None"
        );
    }

    #[test]
    fn error_envelope_serializes() {
        let ee = ErrorEnvelope::new("SessionNotFound", "not found".to_string());
        let json = serde_json::to_value(&ee).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error_type"], "SessionNotFound");
        assert_eq!(json["message"], "not found");
    }

    // -----------------------------------------------------------------------
    // workspace_name_from_path
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_name_from_some_path() {
        let ws = PathBuf::from("/data/projects/my_project");
        let (name, source) = workspace_name_from_path(Some(&ws));
        assert_eq!(name.as_deref(), Some("my_project"));
        assert_eq!(source.as_deref(), Some("session_workspace_path"));
    }

    #[test]
    fn workspace_name_from_none() {
        let (name, source) = workspace_name_from_path(None);
        assert!(name.is_none());
        assert_eq!(source.as_deref(), Some("none"));
    }

    #[test]
    fn workspace_name_from_root_path() {
        // Root path "/" has no file_name, so name should be None.
        let ws = PathBuf::from("/");
        let (name, source) = workspace_name_from_path(Some(&ws));
        assert!(name.is_none());
        assert_eq!(source.as_deref(), Some("none"));
    }
}
