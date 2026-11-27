//! State-aware cross-cage rebind inspection, clean transcript-only migration,
//! and per-agent continuation planning for `bh --auto-rebind`.
//!
//! When an upstream `409 cage_rebind_required` forces a cross-cohort Cage switch
//! mid-run (e.g. Singapore -> US West), this module:
//! 1. Inspects the old Cage's on-disk session & goal state (`claude`, `codex`,
//!    `grok`, `agy`) without leaking any identity files (`.claude.json`,
//!    `installation_id`, `auth.json`, `antigravity-oauth-token`).
//! 2. Classifies the exact interruption state into one of 7 states so `bh`
//!    knows *precisely* what to send (or when NOT to auto-continue, e.g.
//!    human-blocked goals, pending `AskUserQuestion`, or plan approvals).
//! 3. Copies ONLY the pure session transcript (+ single-thread goal row) into
//!    the new account's isolated Cage home.
//! 4. Builds the exact valid CLI argv to resume and continue seamlessly.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::providers::claude_code::project_dir_key;
use crate::sandbox::AccountCageHome;

/// Exhaustive interruption state detected in the old Cage right after a
/// `409 cage_rebind_required` stops an agent attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInterruptState {
    /// Active autonomous `/goal` loop (active, interrupted, usage-limited, or
    /// auto-paused during this run).
    ActiveGoal {
        objective: Option<String>,
        source: &'static str,
    },
    /// Blocked on human decision, user-paused goal, pending `AskUserQuestion`,
    /// or pending plan approval. MUST NOT auto-send `go on` or approve!
    HumanBlockedOrPaused { reason: String },
    /// Local tool(s) finished and persisted `tool_result` / `function_call_output`,
    /// then the follow-up LLM turn hit `409`.
    MidToolLoop { tool_name: Option<String> },
    /// The user's prompt was already persisted to the transcript, then the
    /// initial LLM call hit `409` before any assistant response arrived.
    PendingUserPromptPersisted { prompt_preview: Option<String> },
    /// Failed before any session file was written to disk in the old Cage
    /// (typically Turn 1 pre-persistence). Replay original argv verbatim.
    Turn1Unpersisted,
    /// Interrupted mid-task after an assistant message or `turn_aborted`.
    MidTaskInterrupted,
    /// Session exited cleanly with no active interruption.
    CompletedIdle,
}

/// Snapshot of the latest session discovered in the old Cage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedCageSession {
    pub agent: String,
    pub session_id: Option<String>,
    pub session_path: Option<PathBuf>,
    pub state: SessionInterruptState,
}

/// Concrete action `bh` should take when relaunching in the new Cage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationDecision {
    pub should_auto_continue: bool,
    pub replay_original_args: bool,
    pub continuation_prompt: Option<String>,
    pub uses_slash_goal_resume: bool,
    pub reason: String,
}

/// Snapshots `thread_id`s whose Codex goal was ALREADY `paused` or `blocked`
/// BEFORE this launch started, so we never auto-resume a user-paused or
/// pre-existing human-blocked goal during rebind.
pub fn snapshot_codex_paused_goals(codex_home: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let db_path = codex_home.join("goals_1.sqlite");
    if !db_path.is_file() || is_symlink(&db_path) {
        return out;
    }
    let Ok(conn) = open_sqlite_for_read(&db_path) else {
        return out;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT thread_id FROM thread_goals WHERE LOWER(status) IN ('paused', 'blocked')",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
        for tid in rows.flatten() {
            out.insert(tid);
        }
    }
    out
}

/// Inspects the old Cage home after a `409 cage_rebind_required` interruption
/// and classifies the exact session + interruption state.
pub fn inspect_cage_session(
    agent: &str,
    old_home: &Path,
    workspace: &Path,
    launch_started_at: Option<SystemTime>,
    pre_launch_codex_paused_goals: &BTreeSet<String>,
) -> DetectedCageSession {
    let min_mtime = launch_started_at.map(|t| t.checked_sub(Duration::from_secs(2)).unwrap_or(t));
    match agent {
        "claude" => inspect_claude_cage(old_home, workspace, min_mtime),
        "codex" => inspect_codex_cage(old_home, min_mtime, pre_launch_codex_paused_goals),
        "grok" => inspect_grok_cage(old_home, min_mtime),
        "agy" => inspect_agy_cage(old_home, min_mtime),
        other => DetectedCageSession {
            agent: other.to_string(),
            session_id: None,
            session_path: None,
            state: SessionInterruptState::Turn1Unpersisted,
        },
    }
}

/// Decides what continuation command/prompt (if any) to send for each agent
/// and interruption state, honoring every verified CLI contract:
/// - **Grok `ActiveGoal`**: natively expects `/goal resume`.
/// - **Codex `ActiveGoal`**: goal row is migrated as `active` in `goals_1.sqlite`
///   (passing `/goal resume` on CLI argv is NOT parsed as a slash command by
///   Codex — empirically verified against `codex 0.172.0`!), so we pass a clean
///   goal-continuation prompt that immediately drives the active goal loop.
/// - **Claude `ActiveGoal`**: Claude 2.1.330 has `/goal [<condition> | clear]`
///   (NO `/goal resume` — sending `/goal resume` would overwrite the goal
///   condition with `"resume"`!) and auto-restores its Stop hook from the
///   migrated `.jsonl` `goal_status` attachment. So we send a clean goal
///   continuation prompt, NEVER `/goal resume`.
/// - **AGY `ActiveGoal`**: AGY has `/goal` (NO `/goal resume`) and keeps its
///   `goal_stop_hook` inside the migrated trajectory DB. Send `-i`/`-p` goal
///   continuation prompt, NEVER `/goal resume`.
/// - **`HumanBlockedOrPaused`**: NEVER auto-send `go on` or approve; reopen
///   the migrated session in the new Cage and surface the reason to the user.
pub fn decide_continuation(agent: &str, detected: &DetectedCageSession) -> ContinuationDecision {
    match &detected.state {
        SessionInterruptState::ActiveGoal { objective, source } => {
            if agent == "grok" {
                ContinuationDecision {
                    should_auto_continue: true,
                    replay_original_args: false,
                    continuation_prompt: Some("/goal resume".to_string()),
                    uses_slash_goal_resume: true,
                    reason: format!(
                        "active goal detected ({source}); resuming natively via `/goal resume`"
                    ),
                }
            } else {
                let obj_hint = objective
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let clipped = clip_chars(s, 160);
                        format!(" ({clipped})")
                    })
                    .unwrap_or_default();
                ContinuationDecision {
                    should_auto_continue: true,
                    replay_original_args: false,
                    continuation_prompt: Some(format!(
                        "The previous turn was interrupted during an account/region switch while working toward the active goal{obj_hint}. Continue working toward the goal from the exact point where you left off without repeating completed steps."
                    )),
                    uses_slash_goal_resume: false,
                    reason: format!(
                        "active goal detected ({source}); continuing in new Cage with preserved goal hook/state"
                    ),
                }
            }
        }
        SessionInterruptState::HumanBlockedOrPaused { reason } => ContinuationDecision {
            should_auto_continue: false,
            replay_original_args: false,
            continuation_prompt: None,
            uses_slash_goal_resume: false,
            reason: format!(
                "auto-continue withheld because session awaits human input ({reason})"
            ),
        },
        SessionInterruptState::MidToolLoop { tool_name } => {
            let tool_note = tool_name
                .as_deref()
                .map(|t| format!(" (`{t}`)"))
                .unwrap_or_default();
            ContinuationDecision {
                should_auto_continue: true,
                replay_original_args: false,
                continuation_prompt: Some(format!(
                    "The previous step was interrupted right after tool execution{tool_note} completed due to an upstream account switch. Inspect the last tool output above and continue the task without repeating already-completed tool calls."
                )),
                uses_slash_goal_resume: false,
                reason: format!(
                    "interrupted mid-tool-loop after tool result{tool_note}; resuming with tool-aware continuation"
                ),
            }
        }
        SessionInterruptState::PendingUserPromptPersisted { .. } => ContinuationDecision {
            should_auto_continue: true,
            replay_original_args: false,
            continuation_prompt: Some(
                "The previous turn was interrupted before you could respond due to an upstream account switch. Proceed with the pending user request directly above."
                    .to_string(),
            ),
            uses_slash_goal_resume: false,
            reason: "pending user prompt already persisted in transcript; resuming without duplicating prompt"
                .to_string(),
        },
        SessionInterruptState::Turn1Unpersisted => ContinuationDecision {
            should_auto_continue: true,
            replay_original_args: true,
            continuation_prompt: None,
            uses_slash_goal_resume: false,
            reason: "interrupted before any session file was persisted; replaying original CLI arguments in new Cage"
                .to_string(),
        },
        SessionInterruptState::MidTaskInterrupted => ContinuationDecision {
            should_auto_continue: true,
            replay_original_args: false,
            continuation_prompt: Some(
                "The session was interrupted mid-task due to an upstream account switch. Inspect current workspace state if needed and continue from where you left off without redoing completed work."
                    .to_string(),
            ),
            uses_slash_goal_resume: false,
            reason: "interrupted mid-task; resuming session with workspace-aware continuation"
                .to_string(),
        },
        SessionInterruptState::CompletedIdle => ContinuationDecision {
            should_auto_continue: false,
            replay_original_args: false,
            continuation_prompt: None,
            uses_slash_goal_resume: false,
            reason: "session already idle/completed".to_string(),
        },
    }
}

/// Copies ONLY pure session transcript artifacts (+ single-thread goal row)
/// from `old_home` into `new_home`.
///
/// Security guarantee: NEVER copies `.claude.json` (`userID`/`anonymousId`),
/// `.codex/installation_id`, `.codex/auth.json`, `.codex/thread-writer-locks`,
/// `.gemini/antigravity-cli/antigravity-oauth-token`, or `.grok` auth files.
pub fn migrate_session_for_rebind(
    agent: &str,
    old_home: &AccountCageHome,
    new_home: &AccountCageHome,
    detected: &DetectedCageSession,
) -> Result<Option<String>> {
    let Some(sid) = detected.session_id.clone() else {
        return Ok(None);
    };
    if old_home.home_dir == new_home.home_dir {
        // Same cage directory (rare): still ensure Codex goal row is active if ActiveGoal.
        if agent == "codex"
            && matches!(detected.state, SessionInterruptState::ActiveGoal { .. })
        {
            let _ = migrate_codex_goal_row(
                &old_home.home_dir.join(".codex"),
                &new_home.home_dir.join(".codex"),
                &sid,
                true,
            );
        }
        return Ok(Some(sid));
    }

    let Some(src_path) = detected.session_path.as_deref() else {
        return Ok(None);
    };
    if is_symlink(src_path) {
        anyhow::bail!(
            "refusing to migrate symlinked session artifact {}",
            src_path.display()
        );
    }

    match agent {
        "claude" => {
            let rel = src_path.strip_prefix(&old_home.home_dir).with_context(|| {
                format!(
                    "claude session {} is outside old cage home {}",
                    src_path.display(),
                    old_home.home_dir.display()
                )
            })?;
            let dst = new_home.home_dir.join(rel);
            copy_regular_file_safe(src_path, &dst)?;
            Ok(Some(sid))
        }
        "codex" => {
            let rel = src_path.strip_prefix(&old_home.home_dir).with_context(|| {
                format!(
                    "codex rollout {} is outside old cage home {}",
                    src_path.display(),
                    old_home.home_dir.display()
                )
            })?;
            let dst = new_home.home_dir.join(rel);
            copy_regular_file_safe(src_path, &dst)?;
            let activate_goal =
                matches!(detected.state, SessionInterruptState::ActiveGoal { .. });
            migrate_codex_goal_row(
                &old_home.home_dir.join(".codex"),
                &new_home.home_dir.join(".codex"),
                &sid,
                activate_goal,
            )?;
            Ok(Some(sid))
        }
        "grok" => {
            // `src_path` is `.../.grok/sessions/<cwd>/<uuid>/updates.jsonl`.
            let src_dir = src_path.parent().context("grok session dir missing")?;
            let rel_dir = src_dir.strip_prefix(&old_home.home_dir).with_context(|| {
                format!(
                    "grok session dir {} is outside old cage home {}",
                    src_dir.display(),
                    old_home.home_dir.display()
                )
            })?;
            let dst_dir = new_home.home_dir.join(rel_dir);
            // Allowlist ONLY non-credential session & goal/plan files.
            for rel_file in [
                "updates.jsonl",
                "summary.json",
                "chat_history.jsonl",
                "plan.json",
                "plan_mode.json",
                "goal/state.json",
            ] {
                let s = src_dir.join(rel_file);
                if s.is_file() && !is_symlink(&s) {
                    copy_regular_file_safe(&s, &dst_dir.join(rel_file))?;
                }
            }
            Ok(Some(sid))
        }
        "agy" => {
            let old_agy = old_home.home_dir.join(".gemini").join("antigravity-cli");
            let new_agy = new_home.home_dir.join(".gemini").join("antigravity-cli");
            let src_db = old_agy.join("conversations").join(format!("{sid}.db"));
            if src_db.is_file() && !is_symlink(&src_db) {
                copy_regular_file_safe(&src_db, &new_agy.join("conversations").join(format!("{sid}.db")))?;
            }
            let src_brain = old_agy.join("brain").join(&sid);
            if src_brain.is_dir() && !is_symlink(&src_brain) {
                copy_dir_tree_no_symlinks(&src_brain, &new_agy.join("brain").join(&sid))?;
            }
            Ok(Some(sid))
        }
        _ => Ok(None),
    }
}

/// Builds the exact next CLI argv for `agent` after migrating the session into
/// the new Cage, preserving the user's original execution mode (interactive vs
/// non-interactive `-p`/`exec`) and non-prompt options.
pub fn build_rebind_argv(
    agent: &str,
    original_args: &[String],
    migrated_session_id: Option<&str>,
    decision: &ContinuationDecision,
) -> Vec<String> {
    if decision.replay_original_args {
        return original_args.to_vec();
    }
    let Some(sid) = migrated_session_id.filter(|s| !s.trim().is_empty()) else {
        return original_args.to_vec();
    };
    let prompt = if decision.should_auto_continue {
        decision.continuation_prompt.as_deref()
    } else {
        None
    };

    match agent {
        "claude" => build_claude_rebind_argv(original_args, sid, prompt),
        "codex" => build_codex_rebind_argv(original_args, sid, prompt),
        "grok" => build_grok_rebind_argv(original_args, sid, prompt),
        "agy" => build_agy_rebind_argv(original_args, sid, prompt),
        _ => original_args.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Per-Agent Session Inspection
// ---------------------------------------------------------------------------

fn inspect_claude_cage(
    old_home: &Path,
    workspace: &Path,
    min_mtime: Option<SystemTime>,
) -> DetectedCageSession {
    let projects = old_home.join(".claude").join("projects");
    let ws_dir = projects.join(project_dir_key(workspace));
    let mut candidate = newest_file_matching(&ws_dir, min_mtime, |p| {
        p.extension().and_then(|e| e.to_str()) == Some("jsonl")
    });
    if candidate.is_none() {
        candidate = newest_file_matching(&projects, min_mtime, |p| {
            p.extension().and_then(|e| e.to_str()) == Some("jsonl")
        });
    }
    let Some(path) = candidate else {
        return DetectedCageSession {
            agent: "claude".into(),
            session_id: None,
            session_path: None,
            state: SessionInterruptState::Turn1Unpersisted,
        };
    };
    let sid = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(String::from);

    let Ok(raw) = fs::read_to_string(&path) else {
        return DetectedCageSession {
            agent: "claude".into(),
            session_id: sid,
            session_path: Some(path),
            state: SessionInterruptState::MidTaskInterrupted,
        };
    };

    let mut active_goal: Option<String> = None;
    let mut pending_human_tools: HashMap<String, String> = HashMap::new();
    let mut last_tool_name: Option<String> = None;
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    let mut last_turn = LastTurnKind::None;

    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let entry_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match entry_type {
            "attachment" => {
                if let Some(att) = v.get("attachment").and_then(|x| x.as_object()) {
                    if att.get("type").and_then(|x| x.as_str()) == Some("goal_status") {
                        let met = att.get("met").and_then(|x| x.as_bool()).unwrap_or(false);
                        if met {
                            active_goal = None;
                        } else if let Some(cond) = att.get("condition").and_then(|x| x.as_str())
                            && !cond.trim().is_empty()
                        {
                            active_goal = Some(cond.trim().to_string());
                        }
                    }
                }
            }
            "system" => {
                if v.get("subtype").and_then(|x| x.as_str()) == Some("local_command") {
                    if let Some(content) = v.get("content").and_then(|x| x.as_str()) {
                        if content.contains("Goal cleared") {
                            active_goal = None;
                        }
                    }
                }
            }
            "assistant" => {
                // Claude Code (e.g. v2.1.283) persists synthetic API error bubbles
                // (`isApiErrorMessage: true` / `API Error: 400 Berth Cage rebind...`)
                // as `type: "assistant"` entries after a tool_result or user prompt.
                // Skip them so they never mask `MidToolLoop` or `PendingUserPromptPersisted`!
                let is_api_err = v
                    .get("isApiErrorMessage")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false)
                    || v.get("error").is_some();
                if is_api_err {
                    continue;
                }
                if let Some(content) = v.pointer("/message/content") {
                    if let Some(arr) = content.as_array() {
                        let mut saw_tool_use = false;
                        let mut only_rebind_err_text = !arr.is_empty();
                        for block in arr {
                            if block.get("type").and_then(|x| x.as_str()) == Some("tool_use") {
                                saw_tool_use = true;
                                only_rebind_err_text = false;
                                let id = block
                                    .get("id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !id.is_empty() && !name.is_empty() {
                                    tool_names_by_id.insert(id.clone(), name.clone());
                                }
                                if is_human_blocking_tool(&name) && !id.is_empty() {
                                    pending_human_tools.insert(id, name);
                                }
                            } else if block.get("type").and_then(|x| x.as_str()) == Some("text") {
                                let txt = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                let lower = txt.to_ascii_lowercase();
                                if !lower.contains("cage_rebind_in_progress")
                                    && !lower.contains("berth cage rebind")
                                    && !lower.contains("cage_rebind_required")
                                {
                                    only_rebind_err_text = false;
                                }
                            } else {
                                only_rebind_err_text = false;
                            }
                        }
                        if !saw_tool_use && !only_rebind_err_text {
                            last_turn = LastTurnKind::Assistant;
                        }
                    } else if let Some(s) = content.as_str() {
                        let lower = s.to_ascii_lowercase();
                        if !lower.contains("cage_rebind_in_progress")
                            && !lower.contains("berth cage rebind")
                            && !lower.contains("cage_rebind_required")
                        {
                            last_turn = LastTurnKind::Assistant;
                        }
                    }
                }
            }
            "user" => {
                if let Some(content) = v.pointer("/message/content") {
                    if let Some(arr) = content.as_array() {
                        let mut saw_tool_result = false;
                        let mut text_parts = Vec::new();
                        for block in arr {
                            let bt = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                            if bt == "tool_result" {
                                saw_tool_result = true;
                                if let Some(tid) = block.get("tool_use_id").and_then(|x| x.as_str())
                                {
                                    pending_human_tools.remove(tid);
                                    if let Some(tn) = tool_names_by_id.get(tid) {
                                        last_tool_name = Some(tn.clone());
                                    }
                                }
                            } else if bt == "text"
                                && let Some(t) = block.get("text").and_then(|x| x.as_str())
                            {
                                text_parts.push(t);
                            }
                        }
                        if saw_tool_result {
                            last_turn = LastTurnKind::ToolResult;
                        } else {
                            let joined = text_parts.join(" ");
                            if !is_synthetic_claude_user_text(&joined) {
                                last_turn = LastTurnKind::UserPrompt(clip_chars(joined.trim(), 120));
                            }
                        }
                    } else if let Some(s) = content.as_str() {
                        if !is_synthetic_claude_user_text(s) {
                            last_turn = LastTurnKind::UserPrompt(clip_chars(s.trim(), 120));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let state = if let Some((_, tool)) = pending_human_tools.into_iter().next() {
        SessionInterruptState::HumanBlockedOrPaused {
            reason: format!("Claude is waiting for user response to `{tool}`"),
        }
    } else if let Some(cond) = active_goal {
        SessionInterruptState::ActiveGoal {
            objective: Some(cond),
            source: "claude:goal_status",
        }
    } else {
        match last_turn {
            LastTurnKind::ToolResult => SessionInterruptState::MidToolLoop {
                tool_name: last_tool_name,
            },
            LastTurnKind::UserPrompt(preview) => SessionInterruptState::PendingUserPromptPersisted {
                prompt_preview: (!preview.is_empty()).then_some(preview),
            },
            LastTurnKind::Assistant | LastTurnKind::None => {
                SessionInterruptState::MidTaskInterrupted
            }
        }
    };

    DetectedCageSession {
        agent: "claude".into(),
        session_id: sid,
        session_path: Some(path),
        state,
    }
}

fn inspect_codex_cage(
    old_home: &Path,
    min_mtime: Option<SystemTime>,
    pre_launch_paused_goals: &BTreeSet<String>,
) -> DetectedCageSession {
    let codex_home = old_home.join(".codex");
    let sessions_dir = codex_home.join("sessions");
    let Some(path) = newest_file_matching(&sessions_dir, min_mtime, |p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
    }) else {
        return DetectedCageSession {
            agent: "codex".into(),
            session_id: None,
            session_path: None,
            state: SessionInterruptState::Turn1Unpersisted,
        };
    };

    let Ok(raw) = fs::read_to_string(&path) else {
        return DetectedCageSession {
            agent: "codex".into(),
            session_id: None,
            session_path: Some(path),
            state: SessionInterruptState::MidTaskInterrupted,
        };
    };

    let mut thread_id: Option<String> = None;
    let mut pending_human_calls: HashMap<String, String> = HashMap::new();
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut last_tool_name: Option<String> = None;
    let mut last_turn = LastTurnKind::None;
    let mut model_set_goal_blocked = false;
    let mut last_task_complete_error_is_rebind = false;

    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let entry_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let payload = v.get("payload");
        if entry_type == "session_meta" {
            if let Some(id) = payload
                .and_then(|p| p.get("id").or_else(|| p.get("session_id")))
                .and_then(|x| x.as_str())
                && !id.trim().is_empty()
            {
                thread_id = Some(id.trim().to_string());
            }
            continue;
        }
        if entry_type == "event_msg" {
            if let Some(pl) = payload.and_then(|p| p.as_object())
                && pl.get("type").and_then(|x| x.as_str()) == Some("task_complete")
            {
                let err_msg = pl
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                let lower = err_msg.to_ascii_lowercase();
                last_task_complete_error_is_rebind = lower.contains("cage_rebind_in_progress")
                    || lower.contains("cage_rebind_required")
                    || lower.contains("409 conflict");
            }
            continue;
        }
        if entry_type != "response_item" {
            continue;
        }
        let Some(pl) = payload.and_then(|p| p.as_object()) else {
            continue;
        };
        let pt = pl.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match pt {
            "function_call" | "custom_tool_call" => {
                let cid = pl
                    .get("call_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = pl
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !cid.is_empty() && !name.is_empty() {
                    call_names.insert(cid.clone(), name.clone());
                }
                if name.eq_ignore_ascii_case("update_goal") {
                    if let Some(st) = extract_update_goal_status(pl.get("arguments")) {
                        model_set_goal_blocked = st.eq_ignore_ascii_case("blocked");
                    }
                }
                if is_human_blocking_tool(&name) && !cid.is_empty() {
                    pending_human_calls.insert(cid, name);
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                if let Some(cid) = pl.get("call_id").and_then(|x| x.as_str()) {
                    pending_human_calls.remove(cid);
                    if let Some(n) = call_names.get(cid) {
                        last_tool_name = Some(n.clone());
                    }
                }
                last_turn = LastTurnKind::ToolResult;
            }
            "message" => {
                let role = pl.get("role").and_then(|x| x.as_str()).unwrap_or("");
                if role == "assistant" {
                    last_turn = LastTurnKind::Assistant;
                } else if role == "user" {
                    let txt = extract_codex_message_text(pl.get("content"));
                    let trimmed = txt.trim();
                    if !trimmed.is_empty()
                        && !trimmed.starts_with("<environment_context>")
                        && !trimmed.starts_with("<skills_instructions>")
                        && !trimmed.starts_with("<codex_internal_context")
                    {
                        last_turn = LastTurnKind::UserPrompt(clip_chars(trimmed, 120));
                    }
                }
            }
            _ => {}
        }
    }

    // Check `goals_1.sqlite` for native Codex `/goal` state on this thread!
    if let Some(tid) = thread_id.as_deref() {
        if let Some((status, objective)) = read_codex_goal_row(&codex_home, tid) {
            let st_lower = status.to_ascii_lowercase();
            match st_lower.as_str() {
                "blocked" => {
                    // Empirically verified on Codex 0.172.0: when an active goal turn fails
                    // with our 409/400 `cage_rebind_in_progress`, Codex's internal goal harness
                    // auto-flips `thread_goals.status` to `blocked` ("Goal stalled (/goal resume)")
                    // WITHOUT the model calling `update_goal(status="blocked")`. Distinguish that
                    // synthetic rebind-induced stall from a genuine pre-launch or model-declared
                    // human block!
                    let rebind_stalled = !pre_launch_paused_goals.contains(tid)
                        && !model_set_goal_blocked
                        && last_task_complete_error_is_rebind
                        && pending_human_calls.is_empty();
                    if rebind_stalled {
                        return DetectedCageSession {
                            agent: "codex".into(),
                            session_id: thread_id,
                            session_path: Some(path),
                            state: SessionInterruptState::ActiveGoal {
                                objective: (!objective.trim().is_empty()).then_some(objective),
                                source: "codex:goals_1.sqlite (rebind-stalled)",
                            },
                        };
                    }
                    return DetectedCageSession {
                        agent: "codex".into(),
                        session_id: thread_id,
                        session_path: Some(path),
                        state: SessionInterruptState::HumanBlockedOrPaused {
                            reason: "Codex `/goal` status is `blocked` (awaiting user decision)"
                                .into(),
                        },
                    };
                }
                "paused" if pre_launch_paused_goals.contains(tid) => {
                    return DetectedCageSession {
                        agent: "codex".into(),
                        session_id: thread_id,
                        session_path: Some(path),
                        state: SessionInterruptState::HumanBlockedOrPaused {
                            reason: "Codex `/goal` was already paused by user before this run"
                                .into(),
                        },
                    };
                }
                "active" | "usage_limited" | "budget_limited" | "paused" => {
                    if let Some((_, tool)) = pending_human_calls.into_iter().next() {
                        return DetectedCageSession {
                            agent: "codex".into(),
                            session_id: thread_id,
                            session_path: Some(path),
                            state: SessionInterruptState::HumanBlockedOrPaused {
                                reason: format!("Codex is awaiting user response to `{tool}`"),
                            },
                        };
                    }
                    return DetectedCageSession {
                        agent: "codex".into(),
                        session_id: thread_id,
                        session_path: Some(path),
                        state: SessionInterruptState::ActiveGoal {
                            objective: (!objective.trim().is_empty()).then_some(objective),
                            source: "codex:goals_1.sqlite",
                        },
                    };
                }
                _ => {} // "complete" or unknown -> fall through to normal turn state
            }
        }
    }

    let state = if let Some((_, tool)) = pending_human_calls.into_iter().next() {
        SessionInterruptState::HumanBlockedOrPaused {
            reason: format!("Codex is awaiting user response to `{tool}`"),
        }
    } else {
        match last_turn {
            LastTurnKind::ToolResult => SessionInterruptState::MidToolLoop {
                tool_name: last_tool_name,
            },
            LastTurnKind::UserPrompt(preview) => SessionInterruptState::PendingUserPromptPersisted {
                prompt_preview: (!preview.is_empty()).then_some(preview),
            },
            LastTurnKind::Assistant | LastTurnKind::None => {
                SessionInterruptState::MidTaskInterrupted
            }
        }
    };

    DetectedCageSession {
        agent: "codex".into(),
        session_id: thread_id,
        session_path: Some(path),
        state,
    }
}

fn inspect_grok_cage(old_home: &Path, min_mtime: Option<SystemTime>) -> DetectedCageSession {
    let sessions_dir = old_home.join(".grok").join("sessions");
    let Some(updates_path) = newest_file_matching(&sessions_dir, min_mtime, |p| {
        p.file_name().and_then(|n| n.to_str()) == Some("updates.jsonl")
    }) else {
        return DetectedCageSession {
            agent: "grok".into(),
            session_id: None,
            session_path: None,
            state: SessionInterruptState::Turn1Unpersisted,
        };
    };

    let session_dir = updates_path.parent().unwrap_or(old_home);
    let sid = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from);

    // 1. Check `plan_mode.json` for pending user plan approval.
    let plan_mode_path = session_dir.join("plan_mode.json");
    if plan_mode_path.is_file() && !is_symlink(&plan_mode_path) {
        if let Ok(raw) = fs::read_to_string(&plan_mode_path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            let awaiting = v
                .get("awaiting_approval")
                .or_else(|| v.get("pending_approval"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
                || v.get("status")
                    .or_else(|| v.get("state"))
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| {
                        let l = s.to_ascii_lowercase();
                        l.contains("awaiting") || l.contains("pending_approval")
                    });
            if awaiting {
                return DetectedCageSession {
                    agent: "grok".into(),
                    session_id: sid,
                    session_path: Some(updates_path),
                    state: SessionInterruptState::HumanBlockedOrPaused {
                        reason: "Grok plan mode is awaiting explicit user approval".into(),
                    },
                };
            }
        }
    }

    // 2. Check `goal/state.json` (native Grok `/goal` state machine).
    let goal_state_path = session_dir.join("goal").join("state.json");
    if goal_state_path.is_file() && !is_symlink(&goal_state_path) {
        if let Ok(raw) = fs::read_to_string(&goal_state_path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            let st = v
                .get("status")
                .or_else(|| v.get("state"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let obj = v
                .get("objective")
                .and_then(|x| x.as_str())
                .map(String::from);
            match st.as_str() {
                "user_paused" | "blocked" | "budget_limited" => {
                    return DetectedCageSession {
                        agent: "grok".into(),
                        session_id: sid,
                        session_path: Some(updates_path),
                        state: SessionInterruptState::HumanBlockedOrPaused {
                            reason: format!("Grok `/goal` status is `{st}`"),
                        },
                    };
                }
                "active"
                | "executing"
                | "interrupted"
                | "infra_paused"
                | "back_off_paused"
                | "no_progress_paused"
                | "paused" => {
                    return DetectedCageSession {
                        agent: "grok".into(),
                        session_id: sid,
                        session_path: Some(updates_path),
                        state: SessionInterruptState::ActiveGoal {
                            objective: obj,
                            source: "grok:goal/state.json",
                        },
                    };
                }
                _ => {}
            }
        }
    }

    // 3. Inspect `updates.jsonl` ACP stream.
    let mut last_turn = LastTurnKind::None;
    let mut last_tool_name: Option<String> = None;
    let mut pending_human_tools: HashMap<String, String> = HashMap::new();
    if let Ok(raw) = fs::read_to_string(&updates_path) {
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(upd) = v.pointer("/params/update").and_then(|x| x.as_object()) else {
                continue;
            };
            let kind = upd
                .get("sessionUpdate")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            match kind {
                "user_message_chunk" => {
                    let txt = upd
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    last_turn = LastTurnKind::UserPrompt(clip_chars(txt.trim(), 120));
                }
                "agent_message_chunk" => {
                    last_turn = LastTurnKind::Assistant;
                }
                "tool_call" => {
                    let id = upd
                        .get("toolCallId")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let title = upd
                        .get("title")
                        .or_else(|| upd.get("name"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !title.is_empty() {
                        last_tool_name = Some(title.clone());
                    }
                    if is_human_blocking_tool(&title) && !id.is_empty() {
                        pending_human_tools.insert(id, title);
                    }
                }
                "tool_call_update" => {
                    if let Some(id) = upd.get("toolCallId").and_then(|x| x.as_str()) {
                        pending_human_tools.remove(id);
                    }
                    let status = upd
                        .get("status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if status == "completed" || status == "failed" || status.is_empty() {
                        last_turn = LastTurnKind::ToolResult;
                    }
                }
                _ => {}
            }
        }
    }

    let state = if let Some((_, tool)) = pending_human_tools.into_iter().next() {
        SessionInterruptState::HumanBlockedOrPaused {
            reason: format!("Grok is awaiting user input for `{tool}`"),
        }
    } else {
        match last_turn {
            LastTurnKind::ToolResult => SessionInterruptState::MidToolLoop {
                tool_name: last_tool_name,
            },
            LastTurnKind::UserPrompt(preview) => SessionInterruptState::PendingUserPromptPersisted {
                prompt_preview: (!preview.is_empty()).then_some(preview),
            },
            LastTurnKind::Assistant | LastTurnKind::None => {
                SessionInterruptState::MidTaskInterrupted
            }
        }
    };

    DetectedCageSession {
        agent: "grok".into(),
        session_id: sid,
        session_path: Some(updates_path),
        state,
    }
}

fn inspect_agy_cage(old_home: &Path, min_mtime: Option<SystemTime>) -> DetectedCageSession {
    let agy_root = old_home.join(".gemini").join("antigravity-cli");
    let conv_dir = agy_root.join("conversations");
    let brain_dir = agy_root.join("brain");

    let db_cand = newest_file_matching(&conv_dir, min_mtime, |p| {
        p.extension().and_then(|e| e.to_str()) == Some("db")
    });
    let transcript_cand = newest_file_matching(&brain_dir, min_mtime, |p| {
        p.file_name().and_then(|n| n.to_str()) == Some("transcript.jsonl")
    });

    let (sid, primary_path, transcript_path) = match (&db_cand, &transcript_cand) {
        (Some(db), _) => {
            let id = db.file_stem().and_then(|s| s.to_str()).map(String::from);
            let tr = id
                .as_deref()
                .map(|u| brain_dir.join(u).join(".system_generated").join("logs").join("transcript.jsonl"))
                .filter(|p| p.is_file());
            (id, Some(db.clone()), tr)
        }
        (None, Some(tr)) => {
            // `brain/<uuid>/.system_generated/logs/transcript.jsonl`
            let id = tr
                .strip_prefix(&brain_dir)
                .ok()
                .and_then(|r| r.components().next())
                .and_then(|c| c.as_os_str().to_str())
                .map(String::from);
            (id, Some(tr.clone()), Some(tr.clone()))
        }
        (None, None) => {
            return DetectedCageSession {
                agent: "agy".into(),
                session_id: None,
                session_path: None,
                state: SessionInterruptState::Turn1Unpersisted,
            };
        }
    };

    let mut active_goal: Option<String> = None;
    let mut pending_human_reason: Option<String> = None;
    let mut last_tool_name: Option<String> = None;
    let mut last_turn = LastTurnKind::None;

    if let Some(tr_path) = transcript_path.as_deref()
        && let Ok(raw) = fs::read_to_string(tr_path)
    {
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let source = v.get("source").and_then(|x| x.as_str()).unwrap_or("");
            let step_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");

            if source == "USER_EXPLICIT" {
                pending_human_reason = None;
                let trimmed = unwrap_user_request_tags(content);
                if let Some(rest) = trimmed.strip_prefix("/goal") {
                    let arg = rest.trim();
                    if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("stop") {
                        active_goal = None;
                    } else if !arg.is_empty() {
                        active_goal = Some(arg.to_string());
                    }
                }
                last_turn = LastTurnKind::UserPrompt(clip_chars(trimmed, 120));
            } else if source == "MODEL" {
                let mut saw_tool = false;
                if let Some(calls) = v.get("tool_calls").and_then(|x| x.as_array())
                    && !calls.is_empty()
                {
                    saw_tool = true;
                    for tc in calls {
                        let name = tc.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        if !name.is_empty() {
                            last_tool_name = Some(name.to_string());
                        }
                        if is_human_blocking_tool(name) {
                            pending_human_reason =
                                Some(format!("Antigravity is waiting on user response to `{name}`"));
                        }
                    }
                }
                if !saw_tool {
                    last_turn = LastTurnKind::Assistant;
                }
            } else if step_type.to_ascii_lowercase().contains("tool")
                || status.eq_ignore_ascii_case("DONE") && !source.eq_ignore_ascii_case("SYSTEM")
            {
                if pending_human_reason.is_some()
                    && step_type.to_ascii_lowercase().contains("tool_result")
                {
                    pending_human_reason = None;
                }
                last_turn = LastTurnKind::ToolResult;
            }
        }
    }

    let state = if let Some(reason) = pending_human_reason {
        SessionInterruptState::HumanBlockedOrPaused { reason }
    } else if let Some(obj) = active_goal {
        SessionInterruptState::ActiveGoal {
            objective: Some(obj),
            source: "agy:transcript.jsonl",
        }
    } else {
        match last_turn {
            LastTurnKind::ToolResult => SessionInterruptState::MidToolLoop {
                tool_name: last_tool_name,
            },
            LastTurnKind::UserPrompt(preview) => SessionInterruptState::PendingUserPromptPersisted {
                prompt_preview: (!preview.is_empty()).then_some(preview),
            },
            LastTurnKind::Assistant | LastTurnKind::None => {
                SessionInterruptState::MidTaskInterrupted
            }
        }
    };

    DetectedCageSession {
        agent: "agy".into(),
        session_id: sid,
        session_path: primary_path,
        state,
    }
}

// ---------------------------------------------------------------------------
// Per-Agent Rebind CLI Argument Construction
// ---------------------------------------------------------------------------

fn build_claude_rebind_argv(
    original_args: &[String],
    sid: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let value_flags = [
        "--add-dir",
        "--agent",
        "--agents",
        "--allowedTools",
        "--allowed-tools",
        "--append-system-prompt",
        "--append-system-prompt-file",
        "--autocompact",
        "--betas",
        "--debug-file",
        "--disallowedTools",
        "--disallowed-tools",
        "--effort",
        "--environment",
        "--fallback-model",
        "--input-format",
        "--json-schema",
        "--max-budget-usd",
        "--max-turns",
        "--mcp-config",
        "--model",
        "-n",
        "--name",
        "--output-format",
        "--permission-mode",
        "--permission-prompts",
        "--plugin-dir",
        "--session-id",
        "--settings",
        "--system-prompt",
        "--system-prompt-file",
        "--system-prompt-snapshot",
        "--tools",
    ];
    let drop_with_opt_val = ["-r", "--resume"];
    let drop_bool = ["-c", "--continue"];

    let mut out = strip_prompt_and_resume_flags(
        original_args,
        &value_flags,
        &drop_with_opt_val,
        &drop_bool,
        &[],
    );
    out.push("--resume".to_string());
    out.push(sid.to_string());
    if let Some(p) = prompt {
        // Guard with `--` so variadic flags like `--allowedTools Bash` never eat the prompt.
        out.push("--".to_string());
        out.push(p.to_string());
    }
    out
}

fn build_codex_rebind_argv(
    original_args: &[String],
    sid: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let is_exec = matches!(
        original_args.first().map(String::as_str),
        Some("exec" | "e")
    );
    let value_flags = [
        "-c",
        "--config",
        "--enable",
        "--disable",
        "-i",
        "--image",
        "-m",
        "--model",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-a",
        "--ask-for-approval",
        "-C",
        "--cd",
        "--add-dir",
        "-o",
        "--output-last-message",
        "--output-schema",
        "--color",
        "--remote",
        "--remote-auth-token-env",
    ];

    if is_exec {
        let mut tail = &original_args[1..];
        if tail.first().map(String::as_str) == Some("resume") {
            tail = &tail[1..];
        }
        let preserved = strip_prompt_and_resume_flags(tail, &value_flags, &[], &["--last"], &[]);
        let mut out = vec!["exec".to_string(), "resume".to_string()];
        out.extend(preserved);
        out.push(sid.to_string());
        if let Some(p) = prompt {
            out.push(p.to_string());
        }
        return out;
    }

    let mut slice = original_args;
    if slice.first().map(String::as_str) == Some("resume") {
        slice = &slice[1..];
    }
    let preserved = strip_prompt_and_resume_flags(slice, &value_flags, &[], &["--last"], &[]);
    let mut out = vec!["resume".to_string()];
    out.extend(preserved);
    out.push(sid.to_string());
    if let Some(p) = prompt {
        out.push(p.to_string());
    }
    out
}

fn build_grok_rebind_argv(
    original_args: &[String],
    sid: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let is_single = original_args
        .iter()
        .any(|a| a == "-p" || a == "--single" || a.starts_with("--single="));
    let value_flags = [
        "--add-dir",
        "--agent",
        "--agents",
        "--allow",
        "--allowedTools",
        "--allowed-tools",
        "--cwd",
        "--debug-file",
        "--deny",
        "--disallowedTools",
        "--disallowed-tools",
        "--json-schema",
        "--leader-socket",
        "-m",
        "--model",
        "--max-turns",
        "--output-format",
        "--permission-mode",
        "--prompt-file",
        "--prompt-json",
        "--reasoning-effort",
        "--effort",
        "--rules",
        "-s",
        "--session-id",
        "--sandbox",
        "--system-prompt",
        "--system-prompt-file",
        "--system-prompt-override",
        "--tools",
    ];
    let drop_with_opt_val = ["-r", "--resume", "-p", "--single"];
    let drop_bool = ["-c", "--continue"];

    let mut out = strip_prompt_and_resume_flags(
        original_args,
        &value_flags,
        &drop_with_opt_val,
        &drop_bool,
        &[],
    );
    out.push("--resume".to_string());
    out.push(sid.to_string());
    if let Some(p) = prompt {
        if is_single {
            out.push("-p".to_string());
        }
        out.push(p.to_string());
    }
    out
}

fn build_agy_rebind_argv(
    original_args: &[String],
    sid: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let is_print = original_args.iter().any(|a| {
        matches!(a.as_str(), "-p" | "--print" | "--prompt")
            || a.starts_with("--print=")
            || a.starts_with("--prompt=")
            || a.starts_with("-p=")
    });
    let value_flags = [
        "--add-dir",
        "--agent",
        "--effort",
        "--input-format",
        "--json-schema",
        "--log-file",
        "--mode",
        "--model",
        "--output-format",
        "--print-timeout",
        "--project",
        "--settings",
    ];
    let drop_with_val = [
        "-r",
        "--resume",
        "--conversation",
        "-p",
        "--print",
        "--prompt",
        "-i",
        "--prompt-interactive",
    ];
    let drop_bool = ["-c", "--continue"];

    let mut out = strip_prompt_and_resume_flags(
        original_args,
        &value_flags,
        &drop_with_val,
        &drop_bool,
        &[],
    );
    out.push("--resume".to_string());
    out.push(sid.to_string());
    if let Some(p) = prompt {
        if is_print {
            out.push("-p".to_string());
        } else {
            // `-i` / `--prompt-interactive` runs an initial prompt interactively and keeps the session open.
            out.push("-i".to_string());
        }
        out.push(p.to_string());
    }
    out
}

fn strip_prompt_and_resume_flags(
    args: &[String],
    keep_value_flags: &[&str],
    drop_value_flags: &[&str],
    drop_bool_flags: &[&str],
    _reserved: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            // Everything after `--` is the old positional prompt; drop it.
            break;
        }
        if drop_bool_flags.contains(&a.as_str()) {
            i += 1;
            continue;
        }
        // Drop `--flag=val` variants
        if drop_value_flags
            .iter()
            .any(|f| a.starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }
        if drop_value_flags.contains(&a.as_str()) {
            i += 1;
            if i < args.len() && !args[i].starts_with('-') {
                i += 1;
            }
            continue;
        }
        if keep_value_flags.contains(&a.as_str()) {
            out.push(a.clone());
            i += 1;
            if i < args.len() {
                out.push(args[i].clone());
                i += 1;
            }
            continue;
        }
        if a.starts_with('-') {
            out.push(a.clone());
            i += 1;
            continue;
        }
        // Positional argument (old session ID or old prompt): drop it.
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Codex goals_1.sqlite Single-Thread Migration & Safe File Helpers
// ---------------------------------------------------------------------------

fn open_sqlite_for_read(db_path: &Path) -> Result<Connection> {
    // Prefer normal open + `PRAGMA query_only = ON` so SQLite can cleanly read
    // uncheckpointed WAL pages (`*-wal`) even after `SIGTERM`/`SIGKILL` without
    // modifying logical tables; fall back to strictly read-only open if the
    // directory itself is read-only.
    if let Ok(conn) = Connection::open(db_path) {
        let _ = conn.execute_batch("PRAGMA query_only = ON;");
        return Ok(conn);
    }
    Ok(Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn read_codex_goal_row(codex_home: &Path, thread_id: &str) -> Option<(String, String)> {
    let db_path = codex_home.join("goals_1.sqlite");
    if !db_path.is_file() || is_symlink(&db_path) {
        return None;
    }
    let conn = open_sqlite_for_read(&db_path).ok()?;
    conn.query_row(
        "SELECT status, objective FROM thread_goals WHERE thread_id = ?1",
        [thread_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .ok()
}

fn extract_update_goal_status(args: Option<&serde_json::Value>) -> Option<String> {
    let v = args?;
    if let Some(obj) = v.as_object() {
        return obj
            .get("status")
            .and_then(|s| s.as_str())
            .map(String::from);
    }
    if let Some(raw) = v.as_str()
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw)
    {
        return parsed
            .get("status")
            .and_then(|s| s.as_str())
            .map(String::from);
    }
    None
}

fn migrate_codex_goal_row(
    old_codex_home: &Path,
    new_codex_home: &Path,
    thread_id: &str,
    activate: bool,
) -> Result<()> {
    let old_db = old_codex_home.join("goals_1.sqlite");
    if !old_db.is_file() || is_symlink(&old_db) {
        return Ok(());
    }
    // Open via SQLite (reading any uncheckpointed WAL pages!) to extract the
    // exact authoritative row for `thread_id`.
    let src = open_sqlite_for_read(&old_db)?;
    let row: Option<(String, String, String, Option<i64>, i64, i64, i64, i64)> = src
        .query_row(
            "SELECT goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms
             FROM thread_goals WHERE thread_id = ?1",
            [thread_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .ok();
    let Some((goal_id, objective, old_status, token_budget, tokens_used, time_used, created_ms, updated_ms)) = row else {
        return Ok(());
    };

    let new_db = new_codex_home.join("goals_1.sqlite");
    if is_symlink(&new_db) {
        anyhow::bail!("refusing to write symlinked {}", new_db.display());
    }
    fs::create_dir_all(new_codex_home)?;

    // When creating `new_db` for the first time, materialize a self-contained
    // WAL-free snapshot via `VACUUM INTO` so Codex's `_sqlx_migrations` table
    // and schema constraints are preserved intact even when `old_db` is in WAL
    // mode, then strip every sibling thread's goal rows so ZERO cross-account
    // history leaks.
    if !new_db.exists() {
        let tmp_db = new_db.with_extension(format!(
            "rebind-tmp-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let vacuum_ok = Connection::open(&old_db)
            .and_then(|c| c.execute("VACUUM INTO ?1", [tmp_db.to_string_lossy().as_ref()]))
            .is_ok()
            && tmp_db.is_file();
        if vacuum_ok {
            let mut dst = Connection::open(&tmp_db)?;
            let tx = dst.transaction()?;
            let _ = tx.execute("DELETE FROM thread_goal_continuation_deferrals", []);
            tx.execute("DELETE FROM thread_goals", [])?;
            tx.commit()?;
            drop(dst);
            fs::rename(&tmp_db, &new_db)?;
        } else {
            let _ = fs::remove_file(&tmp_db);
            copy_regular_file_safe(&old_db, &new_db)?;
            let mut dst = Connection::open(&new_db)?;
            let tx = dst.transaction()?;
            let _ = tx.execute("DELETE FROM thread_goal_continuation_deferrals", []);
            tx.execute("DELETE FROM thread_goals", [])?;
            tx.commit()?;
        }
    }

    let dst = Connection::open(&new_db)?;
    dst.execute_batch(
        "CREATE TABLE IF NOT EXISTS thread_goals (
            thread_id TEXT PRIMARY KEY NOT NULL,
            goal_id TEXT NOT NULL,
            objective TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('active','paused','blocked','usage_limited','budget_limited','complete')),
            token_budget INTEGER,
            tokens_used INTEGER NOT NULL DEFAULT 0,
            time_used_seconds INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS thread_goal_continuation_deferrals (
            thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
        );",
    )?;
    let target_status = if activate {
        "active".to_string()
    } else {
        old_status
    };
    dst.execute(
        "INSERT INTO thread_goals (thread_id, goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(thread_id) DO UPDATE SET
           goal_id = excluded.goal_id,
           objective = excluded.objective,
           status = excluded.status,
           token_budget = excluded.token_budget,
           tokens_used = excluded.tokens_used,
           time_used_seconds = excluded.time_used_seconds,
           updated_at_ms = excluded.updated_at_ms",
        rusqlite::params![
            thread_id,
            goal_id,
            objective,
            target_status,
            token_budget,
            tokens_used,
            time_used,
            created_ms,
            updated_ms
        ],
    )?;
    if activate {
        let _ = dst.execute(
            "DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?1",
            [thread_id],
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LastTurnKind {
    None,
    UserPrompt(String),
    ToolResult,
    Assistant,
}

fn is_human_blocking_tool(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "askuserquestion"
            | "ask_user_question"
            | "ask_question"
            | "ask_user"
            | "request_user_input"
            | "exitplanmode"
            | "exit_plan_mode"
            | "request_feedback"
    )
}

fn is_synthetic_claude_user_text(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || t.starts_with("<command-name>")
        || t.starts_with("A session-scoped Stop hook is now active")
        || t.starts_with("<system-reminder>")
}

fn extract_codex_message_text(content: Option<&serde_json::Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            }
        }
        return out;
    }
    String::new()
}

fn unwrap_user_request_tags(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(s) = trimmed.strip_prefix("<USER_REQUEST>") {
        if let Some(inner) = s.strip_suffix("</USER_REQUEST>") {
            return inner.trim();
        }
    }
    trimmed
}

fn clip_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn newest_file_matching<F>(root: &Path, min_mtime: Option<SystemTime>, pred: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    if !root.is_dir() {
        return None;
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if !entry.file_type().is_file() || !pred(p) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(min_t) = min_mtime
            && mtime < min_t
        {
            continue;
        }
        let replace = match &best {
            None => true,
            Some((best_t, _)) => mtime >= *best_t,
        };
        if replace {
            best = Some((mtime, p.to_path_buf()));
        }
    }
    best.map(|(_, p)| p)
}

fn ensure_no_symlink_in_path(root_boundary: Option<&Path>, target: &Path) -> Result<()> {
    let mut cur = match root_boundary {
        Some(r) => r.to_path_buf(),
        None => PathBuf::new(),
    };
    let tail = match root_boundary {
        Some(r) => target.strip_prefix(r).unwrap_or(target),
        None => target,
    };
    for comp in tail.components() {
        cur.push(comp);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing to traverse symlink component {} while writing {}",
                    cur.display(),
                    target.display()
                );
            }
        }
    }
    Ok(())
}

fn copy_regular_file_safe(src: &Path, dst: &Path) -> Result<()> {
    if is_symlink(src) {
        anyhow::bail!("refusing to copy symlink {}", src.display());
    }
    if let Some(parent) = dst.parent() {
        ensure_no_symlink_in_path(None, parent)?;
        fs::create_dir_all(parent)?;
    }
    ensure_no_symlink_in_path(None, dst)?;
    if is_symlink(dst) {
        anyhow::bail!("refusing to overwrite symlink {}", dst.display());
    }
    #[cfg(unix)]
    let bytes = {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(src)?;
        let meta = f.metadata()?;
        if !meta.is_file() {
            anyhow::bail!("refusing to copy non-regular file {}", src.display());
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        buf
    };
    #[cfg(not(unix))]
    let bytes = fs::read(src)?;

    let tmp = dst.with_extension(format!(
        "rebind-tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    {
        use std::io::Write as _;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
    }
    fs::rename(&tmp, dst)?;
    Ok(())
}

fn copy_dir_tree_no_symlinks(src_root: &Path, dst_root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src_root)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if entry.file_type().is_symlink() {
            continue;
        }
        let rel = p.strip_prefix(src_root)?;
        let dst = dst_root.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst)?;
        } else if entry.file_type().is_file() {
            copy_regular_file_safe(p, &dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::berth::LeaseCageProfile;
    use crate::sandbox::prepare_account_cage_home;
    use tempfile::TempDir;

    fn cage_profile(tz: &str, cc: &str, locale: &str) -> LeaseCageProfile {
        LeaseCageProfile {
            cage_id: "cage-1".into(),
            gate_epoch: 1,
            timezone: tz.into(),
            country_code: cc.into(),
            locale: locale.into(),
            accept_language: "en-US,en;q=0.9".into(),
            asn_class: "residential_isp".into(),
            strict_mode: true,
        }
    }

    #[test]
    fn claude_active_goal_never_sends_slash_goal_resume_and_isolates_claude_json() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("proj");
        fs::create_dir_all(&ws).unwrap();
        let old_cage = prepare_account_cage_home(
            tmp.path(),
            Some(23),
            &cage_profile("Asia/Singapore", "SG", "en_SG.UTF-8"),
        )
        .unwrap();
        let new_cage = prepare_account_cage_home(
            tmp.path(),
            Some(24),
            &cage_profile("America/Los_Angeles", "US", "en_US.UTF-8"),
        )
        .unwrap();

        // Plant forbidden identity file in old cage (MUST NOT be migrated!)
        fs::write(
            old_cage.home_dir.join(".claude.json"),
            r#"{"userID":"old_secret_device_id"}"#,
        )
        .unwrap();

        let pdir = old_cage
            .home_dir
            .join(".claude")
            .join("projects")
            .join(project_dir_key(&ws));
        fs::create_dir_all(&pdir).unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let jpath = pdir.join(format!("{sid}.jsonl"));
        fs::write(
            &jpath,
            concat!(
                r#"{"type":"attachment","attachment":{"type":"goal_status","met":false,"condition":"all unit tests pass"}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"run tests"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let detected = inspect_cage_session("claude", &old_cage.home_dir, &ws, None, &BTreeSet::new());
        assert_eq!(detected.session_id.as_deref(), Some(sid));
        assert!(matches!(
            detected.state,
            SessionInterruptState::ActiveGoal { .. }
        ));

        let decision = decide_continuation("claude", &detected);
        assert!(decision.should_auto_continue);
        assert!(!decision.uses_slash_goal_resume);
        let prompt = decision.continuation_prompt.as_deref().unwrap();
        assert!(!prompt.starts_with("/goal resume"));
        assert!(prompt.contains("all unit tests pass"));

        let migrated_id = migrate_session_for_rebind("claude", &old_cage, &new_cage, &detected)
            .unwrap()
            .unwrap();
        assert_eq!(migrated_id, sid);
        assert!(
            new_cage
                .home_dir
                .join(".claude/projects")
                .join(project_dir_key(&ws))
                .join(format!("{sid}.jsonl"))
                .is_file()
        );
        assert!(!new_cage.home_dir.join(".claude.json").exists());

        let argv = build_rebind_argv(
            "claude",
            &[
                "-p".into(),
                "--allowedTools".into(),
                "Bash".into(),
                "--".into(),
                "orig".into(),
            ],
            Some(&migrated_id),
            &decision,
        );
        assert_eq!(argv[0], "-p");
        assert!(argv.contains(&"--allowedTools".to_string()));
        assert!(argv.contains(&"--resume".to_string()));
        assert!(argv.contains(&"--".to_string()));
        assert_eq!(argv.last().unwrap(), prompt);
    }

    #[test]
    fn claude_ask_user_question_withholds_auto_continue() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("proj");
        fs::create_dir_all(&ws).unwrap();
        let pdir = tmp
            .path()
            .join(".claude/projects")
            .join(project_dir_key(&ws));
        fs::create_dir_all(&pdir).unwrap();
        fs::write(
            pdir.join("abc.jsonl"),
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu1","name":"AskUserQuestion","input":{}}]}}"#,
        )
        .unwrap();

        let detected = inspect_cage_session("claude", tmp.path(), &ws, None, &BTreeSet::new());
        assert!(matches!(
            detected.state,
            SessionInterruptState::HumanBlockedOrPaused { .. }
        ));
        let dec = decide_continuation("claude", &detected);
        assert!(!dec.should_auto_continue);
        assert!(dec.continuation_prompt.is_none());
    }

    #[test]
    fn claude_mid_tool_loop_detects_tool_result() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("proj");
        fs::create_dir_all(&ws).unwrap();
        let pdir = tmp
            .path()
            .join(".claude/projects")
            .join(project_dir_key(&ws));
        fs::create_dir_all(&pdir).unwrap();
        fs::write(
            pdir.join("midtool.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"ok"}]}}"#,
                "\n",
                r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 400 Berth Cage rebind is already in progress"}]}}"#,
                "\n",
                r#"{"type":"system","subtype":"api_error"}"#,
                "\n"
            ),
        )
        .unwrap();

        let detected = inspect_cage_session("claude", tmp.path(), &ws, None, &BTreeSet::new());
        assert_eq!(
            detected.state,
            SessionInterruptState::MidToolLoop {
                tool_name: Some("Bash".into())
            }
        );
        let dec = decide_continuation("claude", &detected);
        assert!(dec.should_auto_continue);
        assert!(dec.continuation_prompt.unwrap().contains("`Bash`"));
    }

    #[test]
    fn codex_goal_migration_strips_other_threads_and_activates_only_target() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let old_cage = prepare_account_cage_home(
            tmp.path(),
            Some(23),
            &cage_profile("Asia/Singapore", "SG", "en_SG.UTF-8"),
        )
        .unwrap();
        let new_cage = prepare_account_cage_home(
            tmp.path(),
            Some(24),
            &cage_profile("America/Los_Angeles", "US", "en_US.UTF-8"),
        )
        .unwrap();

        // Forbidden Codex identity files (MUST NOT migrate!)
        fs::write(old_cage.home_dir.join(".codex/installation_id"), "old-uuid").unwrap();
        fs::write(old_cage.home_dir.join(".codex/auth.json"), "{}").unwrap();

        let tid = "01a21abb-0000-7000-8000-000000000001";
        let rdir = old_cage.home_dir.join(".codex/sessions/2026/11/27");
        fs::create_dir_all(&rdir).unwrap();
        fs::write(
            rdir.join(format!("rollout-2026-11-27T10-00-00-{tid}.jsonl")),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{tid}\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"ship feature\"}}]}}}}\n"
            ),
        )
        .unwrap();

        let gdb = old_cage.home_dir.join(".codex/goals_1.sqlite");
        let conn = Connection::open(&gdb).unwrap();
        conn.execute_batch(
            "CREATE TABLE thread_goals (
                thread_id TEXT PRIMARY KEY NOT NULL,
                goal_id TEXT NOT NULL,
                objective TEXT NOT NULL,
                status TEXT NOT NULL,
                token_budget INTEGER,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                time_used_seconds INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE thread_goal_continuation_deferrals (
                thread_id TEXT PRIMARY KEY NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_goals VALUES (?1,'g1','Complete migration','usage_limited',NULL,10,5,1,2)",
            [tid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_goals VALUES ('other-thread','g2','Secret other account goal','active',NULL,0,0,1,2)",
            [],
        )
        .unwrap();
        drop(conn);

        let detected = inspect_cage_session("codex", &old_cage.home_dir, &ws, None, &BTreeSet::new());
        assert_eq!(detected.session_id.as_deref(), Some(tid));
        assert!(matches!(
            detected.state,
            SessionInterruptState::ActiveGoal { .. }
        ));

        let dec = decide_continuation("codex", &detected);
        assert!(dec.should_auto_continue);
        assert!(!dec.uses_slash_goal_resume);

        migrate_session_for_rebind("codex", &old_cage, &new_cage, &detected).unwrap();
        assert!(!new_cage.home_dir.join(".codex/installation_id").exists());
        assert!(!new_cage.home_dir.join(".codex/auth.json").exists());

        let new_conn = Connection::open(new_cage.home_dir.join(".codex/goals_1.sqlite")).unwrap();
        let rows: Vec<(String, String)> = new_conn
            .prepare("SELECT thread_id, status FROM thread_goals")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(rows, vec![(tid.to_string(), "active".to_string())]);

        let argv = build_rebind_argv(
            "codex",
            &[
                "exec".into(),
                "--skip-git-repo-check".into(),
                "ship feature".into(),
            ],
            Some(tid),
            &dec,
        );
        assert_eq!(argv[0], "exec");
        assert_eq!(argv[1], "resume");
        assert!(argv.contains(&"--skip-git-repo-check".to_string()));
        assert!(argv.contains(&tid.to_string()));
    }

    #[test]
    fn codex_pre_launch_paused_goal_is_never_auto_unpaused() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let tid = "01a21abb-0000-7000-8000-000000000099";
        let rdir = tmp.path().join(".codex/sessions/2026/11/27");
        fs::create_dir_all(&rdir).unwrap();
        fs::write(
            rdir.join(format!("rollout-2026-11-27T10-00-00-{tid}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{tid}\"}}}}\n"),
        )
        .unwrap();
        let gdb = tmp.path().join(".codex/goals_1.sqlite");
        let conn = Connection::open(&gdb).unwrap();
        conn.execute_batch(
            "CREATE TABLE thread_goals (thread_id TEXT PRIMARY KEY, goal_id TEXT, objective TEXT, status TEXT, token_budget INTEGER, tokens_used INTEGER, time_used_seconds INTEGER, created_at_ms INTEGER, updated_at_ms INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_goals VALUES (?1,'g1','Obj','paused',NULL,0,0,1,2)",
            [tid],
        )
        .unwrap();
        drop(conn);

        let pre_paused = snapshot_codex_paused_goals(&tmp.path().join(".codex"));
        assert!(pre_paused.contains(tid));

        let detected = inspect_cage_session("codex", tmp.path(), &ws, None, &pre_paused);
        assert!(matches!(
            detected.state,
            SessionInterruptState::HumanBlockedOrPaused { .. }
        ));
        assert!(!decide_continuation("codex", &detected).should_auto_continue);
    }

    #[test]
    fn codex_rebind_stalled_blocked_goal_is_distinguished_from_model_or_prelaunch_block() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let tid = "01a21abb-0000-7000-8000-000000000088";
        let rdir = tmp.path().join(".codex/sessions/2026/11/27");
        fs::create_dir_all(&rdir).unwrap();
        let rpath = rdir.join(format!("rollout-2026-11-27T10-00-00-{tid}.jsonl"));

        let gdb = tmp.path().join(".codex/goals_1.sqlite");
        let conn = Connection::open(&gdb).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE thread_goals (thread_id TEXT PRIMARY KEY, goal_id TEXT, objective TEXT, status TEXT, token_budget INTEGER, tokens_used INTEGER, time_used_seconds INTEGER, created_at_ms INTEGER, updated_at_ms INTEGER);
             CREATE TABLE thread_goal_continuation_deferrals (thread_id TEXT PRIMARY KEY);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_goals VALUES (?1,'g1','Ship overnight','blocked',NULL,0,0,1,2)",
            [tid],
        )
        .unwrap();
        drop(conn);

        // Case 1: Codex itself stalled the active goal because of `cage_rebind_in_progress`.
        fs::write(
            &rpath,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{tid}\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"error\":{{\"message\":\"{{\\\"error\\\":{{\\\"code\\\":\\\"cage_rebind_in_progress\\\"}}}}\"}}}}}}\n"
            ),
        )
        .unwrap();
        let det_stalled = inspect_cage_session("codex", tmp.path(), &ws, None, &BTreeSet::new());
        assert!(matches!(
            det_stalled.state,
            SessionInterruptState::ActiveGoal {
                source: "codex:goals_1.sqlite (rebind-stalled)",
                ..
            }
        ));
        assert!(decide_continuation("codex", &det_stalled).should_auto_continue);

        // Case 2: Pre-launch already `blocked` -> MUST NOT auto-resume even if rebind error occurred!
        let pre_blocked = snapshot_codex_paused_goals(&tmp.path().join(".codex"));
        assert!(pre_blocked.contains(tid));
        let det_pre = inspect_cage_session("codex", tmp.path(), &ws, None, &pre_blocked);
        assert!(matches!(
            det_pre.state,
            SessionInterruptState::HumanBlockedOrPaused { .. }
        ));

        // Case 3: Model explicitly called `update_goal(status="blocked")` -> MUST NOT auto-resume!
        fs::write(
            &rpath,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{tid}\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"call_id\":\"c9\",\"name\":\"update_goal\",\"arguments\":\"{{\\\"status\\\":\\\"blocked\\\"}}\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"error\":{{\"message\":\"cage_rebind_in_progress\"}}}}}}\n"
            ),
        )
        .unwrap();
        let det_model = inspect_cage_session("codex", tmp.path(), &ws, None, &BTreeSet::new());
        assert!(matches!(
            det_model.state,
            SessionInterruptState::HumanBlockedOrPaused { .. }
        ));
    }

    #[test]
    fn grok_active_goal_uses_slash_goal_resume_and_respects_plan_approval() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sdir = tmp.path().join(".grok/sessions/ws/sess-g1");
        fs::create_dir_all(sdir.join("goal")).unwrap();
        fs::write(sdir.join("updates.jsonl"), "{}\n").unwrap();
        fs::write(
            sdir.join("goal/state.json"),
            r#"{"status":"infra_paused","objective":"Fix CI"}"#,
        )
        .unwrap();

        let detected = inspect_cage_session("grok", tmp.path(), &ws, None, &BTreeSet::new());
        let dec = decide_continuation("grok", &detected);
        assert!(dec.should_auto_continue);
        assert!(dec.uses_slash_goal_resume);
        assert_eq!(dec.continuation_prompt.as_deref(), Some("/goal resume"));

        let argv = build_rebind_argv("grok", &["-p".into(), "old".into()], Some("sess-g1"), &dec);
        assert_eq!(argv, vec!["--resume", "sess-g1", "-p", "/goal resume"]);

        // Now add pending plan approval -> MUST withhold auto-continue!
        fs::write(sdir.join("plan_mode.json"), r#"{"awaiting_approval":true}"#).unwrap();
        let blocked = inspect_cage_session("grok", tmp.path(), &ws, None, &BTreeSet::new());
        assert!(matches!(
            blocked.state,
            SessionInterruptState::HumanBlockedOrPaused { .. }
        ));
        assert!(!decide_continuation("grok", &blocked).should_auto_continue);
    }

    #[test]
    fn agy_active_goal_and_turn1_unpersisted_handled_cleanly() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();

        // Empty cage -> Turn1Unpersisted -> replay original args verbatim!
        let empty = inspect_cage_session("agy", tmp.path(), &ws, None, &BTreeSet::new());
        assert_eq!(empty.state, SessionInterruptState::Turn1Unpersisted);
        let dec_empty = decide_continuation("agy", &empty);
        assert!(dec_empty.replay_original_args);
        assert_eq!(
            build_rebind_argv("agy", &["-p".into(), "first ask".into()], None, &dec_empty),
            vec!["-p", "first ask"]
        );

        // Active /goal in AGY transcript -> NEVER sends `/goal resume`, uses `-i` in interactive!
        let conv_dir = tmp.path().join(".gemini/antigravity-cli/conversations");
        let log_dir = tmp
            .path()
            .join(".gemini/antigravity-cli/brain/conv-1/.system_generated/logs");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(conv_dir.join("conv-1.db"), b"sqlite-stub").unwrap();
        fs::write(
            log_dir.join("transcript.jsonl"),
            r#"{"source":"USER_EXPLICIT","content":"<USER_REQUEST>/goal ship release</USER_REQUEST>"}"#,
        )
        .unwrap();

        let detected = inspect_cage_session("agy", tmp.path(), &ws, None, &BTreeSet::new());
        assert_eq!(detected.session_id.as_deref(), Some("conv-1"));
        assert!(matches!(
            detected.state,
            SessionInterruptState::ActiveGoal { .. }
        ));
        let dec = decide_continuation("agy", &detected);
        assert!(dec.should_auto_continue);
        assert!(!dec.uses_slash_goal_resume);
        let argv_tui = build_rebind_argv("agy", &["--model".into(), "gemini-3.1-pro".into()], Some("conv-1"), &dec);
        assert_eq!(argv_tui[0], "--model");
        assert_eq!(argv_tui[2], "--resume");
        assert_eq!(argv_tui[3], "conv-1");
        assert_eq!(argv_tui[4], "-i");
    }
}
