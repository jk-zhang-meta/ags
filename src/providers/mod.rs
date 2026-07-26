//! Provider trait and concrete provider implementations.
//!
//! Each supported provider (Claude Code, Codex, Gemini CLI, Antigravity CLI,
//! Cursor, Cline, Aider, Amp, OpenCode, ChatGPT, ClawdBot, Vibe, Factory,
//! OpenClaw, Pi-Agent, Kiro, Grok Build) implements the [`Provider`] trait to
//! read/write sessions in its native format.

pub mod aider;
pub mod amp;
pub mod antigravity;
pub mod chatgpt;
pub mod claude_code;
pub mod claude_code_ir;
pub mod claude_code_ir_write;
pub mod clawdbot;
pub mod cline;
pub mod codex;
pub mod codex_ir;
pub mod codex_ir_write;
pub mod cursor;
pub mod factory;
pub mod gemini;
pub mod grok;
pub mod kiro;
pub mod openclaw;
pub mod opencode;
pub mod pi_agent;
pub mod vibe;

use std::path::{Path, PathBuf};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::ir::{Fidelity, Loss, SessionIr};
use crate::model::CanonicalSession;

/// Options controlling how a session is written to disk.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Overwrite existing session file (creates `.bak` backup).
    pub force: bool,
}

/// Describes the files produced by a successful write operation.
#[derive(Debug, Clone)]
pub struct WrittenSession {
    /// Paths of files written.
    pub paths: Vec<PathBuf>,
    /// Session ID in the target provider's format.
    pub session_id: String,
    /// Ready-to-paste command to resume the session.
    pub resume_command: String,
    /// Path to the `.bak` backup, if an existing file was overwritten.
    pub backup_path: Option<PathBuf>,
    /// Non-fatal warnings produced while writing (e.g. the target session was
    /// written but could not be registered in the provider's resume index).
    /// Surfaced to the user and merged into the conversion's warning list.
    pub warnings: Vec<String>,
}

/// The result of a structured write: the files, plus how much of the session
/// actually survived into them.
///
/// The grade travels with the write rather than being inferred afterwards
/// because only the writer knows what it had to leave behind. A caller looking
/// at the output files cannot tell a conversion that dropped reasoning from one
/// that dropped the conversation, and those two have very different
/// consequences for the person about to resume the session.
///
/// Kept separate from [`WrittenSession`] so that adding it costs the flat path
/// nothing: the seventeen providers that construct a `WrittenSession` directly
/// are untouched.
#[derive(Debug, Clone)]
pub struct StructuredWrite {
    pub written: WrittenSession,
    /// The worst grade any part of this conversion earned.
    pub fidelity: Fidelity,
    /// What that grade is made of, one entry per kind of loss.
    ///
    /// A grade on its own is not actionable — "history incomplete" does not
    /// tell the user how much is gone or why. These carry the counts so the
    /// launch refusal can say what it is refusing about, and so a machine
    /// consumer can filter on [`LossKind`] instead of parsing prose.
    pub losses: Vec<Loss>,
}

/// The core abstraction each provider implements.
///
/// Object-safe so we can store `Box<dyn Provider>` in the registry.
pub trait Provider: Send + Sync {
    /// Human-readable name (e.g. `"Claude Code"`).
    fn name(&self) -> &str;

    /// Short slug used in session metadata (e.g. `"claude-code"`).
    fn slug(&self) -> &str;

    /// CLI alias used in `casr <alias> resume …` (e.g. `"cc"`).
    fn cli_alias(&self) -> &str;

    /// Probe whether this provider is installed on the machine.
    fn detect(&self) -> DetectionResult;

    /// Root directories where this provider stores sessions.
    fn session_roots(&self) -> Vec<PathBuf>;

    /// Check if `session_id` belongs to this provider; return the file path if so.
    fn owns_session(&self, session_id: &str) -> Option<PathBuf>;

    /// Read a session from its native format into canonical IR.
    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession>;

    /// Write a canonical session into this provider's native format.
    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession>;

    /// Build the shell command to resume a session with this provider.
    ///
    /// Display form. To actually start the agent, use
    /// [`Provider::launch_spec`], which is what gets executed.
    fn resume_command(&self, session_id: &str) -> String;

    /// The process to start, structured rather than rendered.
    ///
    /// The default recovers a spec by splitting [`Provider::resume_command`],
    /// which is quote-aware and correct for every provider whose resume form
    /// is a plain command line — so no provider has to be rewritten to gain a
    /// launcher. A provider overrides this when it needs a working directory,
    /// an environment override, or an argument that does not survive being
    /// rendered to a string and split back apart.
    ///
    /// Returns `None` only when the rendered command cannot be split at all,
    /// which is a bug in that provider's `resume_command`.
    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        LaunchSpec::from_command_line(&self.resume_command(session_id))
            .map(|spec| spec.targeting_session(session_id))
    }

    /// Enumerate all discoverable sessions for this provider.
    ///
    /// Returns `Some(vec)` of `(session_id, path)` pairs when the provider
    /// stores multiple sessions in a single file or database and directory
    /// walking alone would undercount.  The default returns `None`, which
    /// tells the caller to fall back to directory walking + `read_session`.
    fn list_sessions(&self) -> Option<Vec<(String, PathBuf)>> {
        None
    }

    // -- High-fidelity track ------------------------------------------------
    //
    // The two methods below are the structured counterparts of `read_session`
    // and `write_session`. They exist because flattening a session to text is
    // the right trade for most providers and the wrong one for Codex and
    // Claude Code, where reasoning capsules, compaction, and tool protocol all
    // matter (see `crate::ir`).
    //
    // Both default to "not supported" so that a provider opts in by overriding
    // rather than by being rewritten. A provider that does not override keeps
    // the flat path unchanged, and the pipeline labels its conversions with a
    // correspondingly lower `Fidelity`.

    /// Read a session into the structured IR.
    ///
    /// `Ok(None)` means "this provider has no structured reader" and is not an
    /// error — the caller falls back to [`Provider::read_session`].
    fn read_session_ir(&self, _path: &Path) -> anyhow::Result<Option<SessionIr>> {
        Ok(None)
    }

    /// Whether [`Provider::write_session_ir`] can do anything.
    ///
    /// A capability probe, separate from the method itself, because
    /// `write_session_ir` cannot be asked whether it exists without first being
    /// handed an IR — and building that IR means parsing the source session a
    /// second time, 281 MiB for the largest rollout in the corpus, only to be
    /// told `Ok(None)`. Nineteen of the twenty-one providers answer `false`.
    ///
    /// `true` is a claim about the writer's existence, not about any particular
    /// session: a provider that supports the track still returns `Ok(None)`
    /// from `write_session_ir` for a session whose replay is empty.
    fn supports_structured_write(&self) -> bool {
        false
    }

    /// Write a structured IR into this provider's native format.
    ///
    /// `Ok(None)` means "this provider has no structured writer"; the caller
    /// falls back to [`Provider::write_session`] with the flat projection.
    ///
    /// Implementors must honour [`SessionIr::model_visible`] rather than
    /// emitting `events` verbatim: replaying compacted-away history is how a
    /// converted session ends up larger than the original conversation.
    fn write_session_ir(
        &self,
        _ir: &SessionIr,
        _opts: &WriteOptions,
    ) -> anyhow::Result<Option<StructuredWrite>> {
        Ok(None)
    }
}
