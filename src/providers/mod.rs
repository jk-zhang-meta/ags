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
pub mod pi_session;
pub mod vibe;

use std::path::{Path, PathBuf};

use crate::budget::ContextBudget;
use crate::discovery::DetectionResult;
use crate::ir::{Fidelity, Loss, SessionIr};
use crate::launch::LaunchSpec;
use crate::model::CanonicalSession;

/// A place a listing had to read and could not, with the reason.
///
/// # Why an empty listing needs this
///
/// Nine provider files reached an empty listing from an I/O *error*, in four
/// spellings, all silent: `read_dir(..).into_iter().flatten().flatten()`,
/// `let Ok(entries) = .. else { return .. }`, `Err(_) => return Some(vec![])`,
/// and a bare `if let Ok(entries) = ..`. A directory owned by another user, a
/// store path a stray file has taken over, a disk returning `EIO` — every one
/// of them produced the same answer as a directory that is genuinely empty, and
/// the user was told "no sessions". `cmd_list`'s own fallback walk made the
/// tenth, with `filter_map(Result::ok)`.
///
/// One of the nine, `Gemini::session_roots`, is deliberately left alone: `list`
/// never reaches it, because Gemini enumerates itself. Its remaining callers
/// ask "does this provider own this path", where an empty answer already means
/// "no" — the same reason `owns_session` keeps its `.ok()?`.
///
/// A missing directory is *not* one of these. It is the ordinary state of a
/// provider that has never run, and reporting it would put a line of noise in
/// front of every user of every tool they have installed but not used. See
/// [`read_dir_reporting`], which draws that line once so seventeen providers do
/// not each draw it differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableSource {
    /// The directory or file that could not be read.
    pub path: PathBuf,
    /// The operating system's reason, verbatim.
    pub error: String,
}

/// What a provider's listing found, and what it could not look at.
///
/// The pair is the point, and it is the same argument [`Displaced`] makes: a
/// result that does not say what it could not measure is indistinguishable
/// from a complete one. `sessions` alone cannot carry "and there may be more
/// in the directory I was refused" — a short list and a whole list are the same
/// value.
///
/// `unreadable` is empty on the ordinary run. It is not a warning channel for
/// files that are simply not sessions: a provider that knows a file is not one
/// of its own excludes it silently, because saying so on every run would bury
/// the cases that mean something. It is for places the provider *expected* to
/// read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListing {
    /// `(session_id, path)` for every session found.
    pub sessions: Vec<(String, PathBuf)>,
    /// Every directory or file the listing could not read, and why.
    pub unreadable: Vec<UnreadableSource>,
}

impl SessionListing {
    /// Record a place this listing could not read.
    pub fn cannot_read(&mut self, path: &Path, error: &std::io::Error) {
        self.unreadable.push(UnreadableSource {
            path: path.to_path_buf(),
            error: error.to_string(),
        });
    }
}

/// Enumerate `dir`, recording the failure instead of returning nothing.
///
/// The one place that decides which failures are worth telling the user about:
///
/// * [`std::io::ErrorKind::NotFound`] is not a failure. A provider's store
///   directory does not exist until the tool has run, and every provider casr
///   detects by binary-in-`PATH` has that state on a fresh install. It yields
///   an empty list and no entry.
/// * Everything else is recorded — the `EACCES` of a directory owned by
///   another user, the `ENOTDIR` of a store path a regular file has taken over,
///   the `EIO` of a failing mount.
///
/// Per-entry errors are recorded too. `entries.flatten()` discards them, and a
/// directory that lists ten files and errors on the eleventh is exactly as
/// silent as one that could not be opened at all.
pub fn read_dir_reporting(
    dir: &Path,
    unreadable: &mut Vec<UnreadableSource>,
) -> Vec<std::fs::DirEntry> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                unreadable.push(UnreadableSource {
                    path: dir.to_path_buf(),
                    error: error.to_string(),
                });
            }
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => out.push(entry),
            Err(error) => unreadable.push(UnreadableSource {
                path: dir.to_path_buf(),
                error: format!("entry in this directory could not be read: {error}"),
            }),
        }
    }
    out
}

/// A line of [`Provider::detect`] evidence naming a directory `list` will read
/// and saying whether it can be read.
///
/// # Why detection has to name the store
///
/// `detect` answers "is this tool on this machine" and `list` answers "what is
/// in its store", and most providers answer the first from something that does
/// not imply the second — a binary in `PATH`, or a *parent* of the store.
/// `~/.claude` exists on a machine that has never run Claude Code; `~/.codex`
/// exists as soon as a config is written; `~/.grok/bin/grok` is what the
/// installer puts there before any session runs.
///
/// So the user sees `✓ Claude Code — installed` from one command and no rows
/// from the other, with nothing in either output saying which of three
/// different things happened: there are no sessions yet, casr is reading a
/// directory that does not exist, or casr was refused. Those want three
/// different responses and were rendered identically.
///
/// This is the same shape `opencode` already uses for each database it finds,
/// and it is evidence rather than a narrowing of `detect` on purpose:
/// "installed" is a true and useful answer about a tool whose store is empty,
/// and a `detect` that required the store would report `✗` for a CLI the user
/// can run right now.
pub fn store_evidence(store: &Path) -> String {
    match std::fs::read_dir(store) {
        Ok(_) => format!("session store: {}", store.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => format!(
            "no session store at {} yet; `list` has nothing to read",
            store.display()
        ),
        Err(error) => format!(
            "{}: UNREADABLE — {error}; no sessions can be listed from it",
            store.display()
        ),
    }
}

/// Keep one entry of a [`walkdir`] walk, or record why the walker stopped
/// there.
///
/// The recursive counterpart of [`read_dir_reporting`], and it draws the same
/// line for the same reason: a walk whose root does not exist yields one
/// `NotFound` error, which is the ordinary state of an uninstalled provider
/// and not worth a line of output. Every other error means a subtree was
/// skipped — `walkdir` reports it and keeps walking, so the sessions it *did*
/// reach still make it into the listing.
///
/// `filter_map(Result::ok)`, which this replaces, made that subtree vanish.
pub fn walk_entry_reporting(
    entry: Result<walkdir::DirEntry, walkdir::Error>,
    unreadable: &mut Vec<UnreadableSource>,
) -> Option<walkdir::DirEntry> {
    match entry {
        Ok(entry) => Some(entry),
        Err(error) => {
            let missing = error
                .io_error()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
            if !missing {
                unreadable.push(UnreadableSource {
                    path: error
                        .path()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("<unknown>")),
                    error: match error.io_error() {
                        Some(io) => io.to_string(),
                        None => error.to_string(),
                    },
                });
            }
            None
        }
    }
}

/// Convert a canonical id into one filename-safe target-provider id.
///
/// A canonical id can legitimately contain separators (Codex rollouts do),
/// but a flat provider store must never interpret those separators as path
/// components. Writers use the returned value consistently for the filename,
/// native header, and resume command.
pub(crate) fn filename_safe_session_id(session_id: &str) -> String {
    urlencoding::encode(session_id).into_owned()
}

/// Options controlling how a session is written to disk.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Overwrite existing session file (creates `.bak` backup).
    pub force: bool,
}

/// A file this write displaced, and where its previous contents were put.
///
/// The pair is the point. It replaced a bare `backup_path: Option<PathBuf>`
/// that the rollback in [`crate::pipeline`] paired with `paths[0]` by
/// assumption — true for the eleven providers that write one file, and false
/// for the two that do not. Cline's only backup is of `state/taskHistory.json`,
/// a *shared* index that is not in `paths` at all, so a rollback moved the
/// global task index on top of `api_conversation_history.json` and reported
/// success. A backup that does not say what it restores is not a backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Displaced {
    /// The file that was overwritten, and where the backup goes back to.
    pub target: PathBuf,
    /// Where its previous contents live until the write is accepted.
    pub backup: PathBuf,
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
    /// Every file this write overwrote, each with the backup that restores it.
    ///
    /// Empty when nothing was displaced, which is the ordinary case: a session
    /// is written to a fresh path unless `--force` aimed it at an existing one.
    pub backups: Vec<Displaced>,
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
    ///
    /// `session_id` is an identifier, never a filesystem path. Implementors
    /// build a candidate by joining it onto one of their own roots, and
    /// [`std::path::Path::join`] discards the receiver when the argument is
    /// absolute — so an absolute path arriving here is returned verbatim and
    /// the provider claims a file it has never owned. Three of the registered
    /// providers were doing exactly that.
    ///
    /// [`crate::discovery::ProviderRegistry`] rejects absolute paths and `..`
    /// components before any implementor sees them, which is why implementors
    /// need no guard of their own. Resolve a real path with
    /// [`crate::discovery::SourceHint::Path`].
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

    /// Enumerate all discoverable sessions for this provider, and every place
    /// it could not read.
    ///
    /// # The three answers, and what each one means
    ///
    /// * **`None` — "I do not enumerate myself."** The caller walks
    ///   [`Provider::session_roots`] instead and filters with
    ///   [`Provider::is_session_path`]. It is a statement about this
    ///   *implementation*, never about the store, and it must not be returned
    ///   because something went wrong. Five of the seventeen registered
    ///   providers answer this way; the walk reports their read failures on
    ///   their behalf, through [`walk_entry_reporting`], so choosing `None`
    ///   costs no reporting.
    /// * **`Some` with an empty `sessions` and an empty `unreadable` — "I read
    ///   everything I meant to and there is nothing here."** This is the
    ///   ordinary state of an installed tool that has not been run, and it must
    ///   stay quiet: see [`read_dir_reporting`], which is why a store directory
    ///   that does not exist is not an error.
    /// * **`Some` with a non-empty `unreadable` — "these specific places I
    ///   expected to read, I could not."** Any session inside them is missing
    ///   from `sessions`. It is *not* all-or-nothing: a provider reports what it
    ///   reached and what it did not, in the same value, because one refused
    ///   directory must not delete the sessions found in the others.
    ///
    /// # Why the listing carries its own failures
    ///
    /// Because nothing downstream can reconstruct them. `cmd_list` sees a
    /// `Vec` and cannot tell a provider that read its store and found it empty
    /// from one that was refused at the door, and those are the two facts a
    /// user staring at `✓ installed` and zero rows is trying to choose
    /// between. Only the enumeration knows.
    ///
    /// Implementors: build the vector with [`read_dir_reporting`] or
    /// [`walk_entry_reporting`], never with
    /// `read_dir(..).into_iter().flatten().flatten()`, `let Ok(entries) = ..
    /// else`, `Err(_) => return Some(vec![])`, or a bare `if let Ok(entries)`.
    /// Those four spellings are how nine providers came to answer "zero
    /// sessions" to an `EACCES`.
    fn list_sessions(&self) -> Option<SessionListing> {
        None
    }

    /// Whether `path`, found under one of this provider's
    /// [`Provider::session_roots`], is a file this tool writes sessions to.
    ///
    /// Consulted by the fallback walk in `cmd_list` for the providers that
    /// answer `None` to [`Provider::list_sessions`], and by six of the
    /// providers that do not, from inside their own enumeration. Without it the
    /// walk hands every file it finds to `read_session`, which is how
    /// ClawdBot's `sessions.json`, Factory's `<sessionId>.settings.json` and
    /// Vibe's `meta.json` each came to be rendered as a session with zero
    /// messages.
    ///
    /// The rule must be the tool's own — the extension it writes, the filename
    /// it fixes, the layout it uses — and taken from the shipped artifact. A
    /// list of names *not* to show is the same defect with a different sign: it
    /// excludes the one file someone remembered and admits the next one the
    /// tool adds.
    ///
    /// The default is the blanket extension set `cmd_list` used to apply to
    /// everything. It exists only so the three mock providers in the test suite
    /// need not answer a question they have no store for; every registered
    /// provider overrides it, which
    /// `list_truthfulness_test::every_registered_provider_narrows_the_default_session_file_rule`
    /// checks rather than assumes.
    fn is_session_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("jsonl" | "json" | "vscdb" | "md" | "db" | "sqlite")
        )
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

    /// Whether [`Provider::read_session_ir`] can do anything.
    ///
    /// The read-side companion of [`Provider::supports_structured_write`], and
    /// it exists for the same reason: a capability is a property of the
    /// provider, so asking about it should not cost a session parse. Every
    /// provider but codex and claude-code answers `false`.
    ///
    /// It buys less than the write-side probe does, and the gap is worth
    /// stating because it bounds where the probe may be used. Skipping
    /// `write_session_ir` skips building an IR the writer would refuse;
    /// skipping `read_session_ir` on a provider that has no reader skips a call
    /// that was already going to return `Ok(None)` without touching the file.
    /// The 281 MiB parse it looks like it should save belongs to the two
    /// providers that answer `true`, and for those the parse cannot be skipped:
    /// `pipeline::flat_fidelity` needs the same IR to see a sealed compaction
    /// the flat projection is about to delete, whatever the target is. Gating
    /// the read on the *target's* capability would buy one parse and sell that
    /// grade — see the note at the pipeline's track selection.
    ///
    /// `true` is a claim about the reader's existence, not about any particular
    /// file: a provider that supports the track still returns `Err` for a
    /// session it cannot parse.
    fn supports_structured_read(&self) -> bool {
        false
    }

    /// Whether [`Provider::write_session_ir`] can do anything.
    ///
    /// A capability probe, separate from the method itself, because
    /// `write_session_ir` cannot be asked whether it exists without first being
    /// handed an IR — and building that IR means parsing the source session a
    /// second time, 281 MiB for the largest rollout in the corpus, only to be
    /// told `Ok(None)`. Every provider but codex and claude-code answers `false`.
    ///
    /// `true` is a claim about the writer's existence, not about any particular
    /// session: a provider that supports the track still returns `Ok(None)`
    /// from `write_session_ir` for a session whose replay is empty.
    fn supports_structured_write(&self) -> bool {
        false
    }

    /// Write a structured IR into this provider's native format, inside
    /// `budget`.
    ///
    /// `Ok(None)` means "this provider has no structured writer"; the caller
    /// falls back to [`Provider::write_session`] with the flat projection.
    ///
    /// Implementors must honour [`SessionIr::model_visible`] rather than
    /// emitting `events` verbatim: replaying compacted-away history is how a
    /// converted session ends up larger than the original conversation.
    ///
    /// # Why the budget is a parameter here and not in [`WriteOptions`]
    ///
    /// The flat track never sees one. Its budget is applied to the
    /// [`CanonicalSession`] in the pipeline, before any writer is called, so a
    /// `budget` field on `WriteOptions` would hand every flat writer a
    /// value that exactly one of the two tracks is expected to act on — an
    /// option that is silently ignored by most of its implementors is a bug
    /// waiting for its first reader. The structured writers cannot be served the
    /// same way: what they write is the IR, and trimming an IR before the writer
    /// means rebuilding one, which is the [`crate::replay`] fold's answer to
    /// override rather than reuse.
    ///
    /// Implementors must apply it with [`crate::budget::ContextBudget::apply`]
    /// over `model_visible`, and must fold what it removes into the losses
    /// [`StructuredWrite::fidelity`] is derived from.
    /// [`crate::budget::ContextBudget::UNLIMITED`] must produce byte-identical
    /// output to a writer that had no budget at all — and since the caps are
    /// opt-in, `UNLIMITED` is what a plain `resume` hands you, so that is the
    /// ordinary path rather than a corner. It is checked rather than assumed:
    /// both writers were run over all 831 readable corpus sessions against a
    /// build with the `apply` call physically deleted, and all 1,662 renders
    /// matched.
    fn write_session_ir(
        &self,
        _ir: &SessionIr,
        _opts: &WriteOptions,
        _budget: &ContextBudget,
    ) -> anyhow::Result<Option<StructuredWrite>> {
        Ok(None)
    }

    /// The grade [`Provider::write_session_ir`] would earn on `ir` inside
    /// `budget`, without writing anything.
    ///
    /// `Ok(None)` in exactly the cases `write_session_ir` returns it — no
    /// structured writer, or a replay with nothing in it — so a caller can use
    /// the two interchangeably to decide which track a conversion takes.
    ///
    /// # Why this exists
    ///
    /// `--dry-run` answers "what will this conversion cost me before I let it
    /// write", and it was answering with [`crate::pipeline::flat_fidelity`] on
    /// every conversion, budget excluded — so the user deciding whether a
    /// `--max-context-tokens` value was survivable got a grade from a code path
    /// their real run would not take. Only the writer knows what it would have
    /// to leave behind, and the writer cannot be asked without being run.
    ///
    /// It is answerable because the expensive half is pure: both structured
    /// providers build the whole file in memory (`codex_ir_write::render`,
    /// `claude_code_ir_write::render`) and only then place it on disk, and the
    /// grade is settled by the first half. Implementors must return the grade
    /// from *that same rendering call*, not re-derive it: two ways to compute
    /// one fact is how `codex_ir_write::Writer::summarise` came to disagree with
    /// itself. Neither the target session id nor the clock reaches
    /// `fidelity`/`losses`, so a placeholder for both is honest here.
    ///
    /// A provider that overrides `write_session_ir` and leaves this at the
    /// default will convert correctly and mis-report every dry run of itself.
    fn grade_session_ir(
        &self,
        _ir: &SessionIr,
        _budget: &ContextBudget,
    ) -> anyhow::Result<Option<(Fidelity, Vec<Loss>)>> {
        Ok(None)
    }
}
