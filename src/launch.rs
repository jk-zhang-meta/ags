//! Starting the target agent, rather than printing a command to paste.
//!
//! Upstream's contract ends at a `String`: [`crate::providers::Provider::resume_command`]
//! renders something like `claude --resume <id>` and the user copies it. That
//! is fine as documentation and wrong as a mechanism. A shell string cannot
//! carry a working directory, cannot carry environment overrides, and cannot
//! be executed without either handing it to a shell — which re-parses
//! everything and makes quoting a correctness problem — or splitting it back
//! apart and hoping the split matches the intent.
//!
//! [`LaunchSpec`] is the structured form: an executable and an argument
//! vector that go straight to `execve` with no shell in between. The string is
//! derived *from* the spec for display, so the thing shown to the user and the
//! thing actually run cannot drift apart.
//!
//! # Passthrough
//!
//! Users want to start a resumed session with their own flags — a different
//! model, a permission mode, an approval policy. Those are appended after the
//! resume arguments, and [`LaunchSpec::try_passthrough`] refuses any flag the
//! spec already sets. That check exists because the failure it prevents is
//! silent: passing `--resume <other-id>` alongside a conversion would start a
//! different session than the one just written, and nothing in the output
//! would say so.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::{Record, SessionKey};

/// The provider session a store record id stands for, when the target is
/// `target_slug`.
///
/// `agsx resume cc <record-id>` has to become a provider session somewhere: the
/// record id is ours, and no agent has ever heard of it. So the identifier the
/// user has — one id for the whole conversation, however many providers it has
/// been through — is translated here into the one the pipeline resolves and the
/// agent can be pointed at.
///
/// The target's own incarnation comes first, and that ordering is the whole
/// content of this function. It needs no conversion, so `--launch` starts the
/// agent on the session it already has; the store's own ranking then still runs
/// over the record and may prefer a better source anyway, which is why this only
/// has to name *a* session of the conversation rather than the best one. The
/// origin is the fallback, because it is the one incarnation every record has.
///
/// `None` for a record with no incarnations at all, which `fsck` reports as a
/// broken record rather than something to launch.
pub fn session_named_by_record<'a>(
    record: &'a Record,
    target_slug: &str,
) -> Option<&'a SessionKey> {
    record
        .for_provider(target_slug)
        .or_else(|| record.origin())
        .or_else(|| record.incarnations.first())
        .map(|incarnation| &incarnation.key)
}

/// Everything needed to start an agent on a converted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Executable name or path. Resolved against `PATH` by the OS, not a shell.
    pub program: String,
    /// Arguments, already split. Never re-parsed.
    pub args: Vec<String>,
    /// Directory to start the agent in.
    ///
    /// Matters more than it looks: both agents scope their session lookup and
    /// their project context by working directory, so launching from the
    /// wrong one can produce an agent that cannot find the session that was
    /// just written for it.
    pub cwd: Option<PathBuf>,
    /// Environment overrides layered on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// The session this command actually names, when it names one.
    ///
    /// See [`SessionTargeting`]. Set by [`LaunchSpec::targeting_session`]
    /// rather than assumed, because for three providers in the registry it is
    /// genuinely `None`.
    pub targets: Option<String>,
}

/// Whether launching this spec resumes the session that was just written.
///
/// Not a detail. The promise of a launcher is "we start you where you left
/// off", and for part of the registry that promise cannot be kept: Cursor's
/// IDE composer (`cursor .`) and Cline (`code .`) open an editor without
/// naming the converted session, while Aider's `--restore-chat-history`
/// restores whichever chat was last rather than a named one. The conversion
/// still writes a correct file — the agent simply will not be pointed at it.
///
/// Modelling this means the caller has to decide what to tell the user, which
/// is the point. The alternative is launching the agent, having it open
/// something else, and letting the user work out why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTargeting {
    /// The command names the session. Launching resumes exactly it.
    ById,
    /// The command starts the agent but does not name a session.
    NotTargeted,
}

impl LaunchSpec {
    /// A spec for `program` with `args`.
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            cwd: None,
            env: Vec::new(),
            targets: None,
        }
    }

    /// Record which session this command names, if it names it at all.
    ///
    /// Detected rather than asserted: the id has to appear in the arguments
    /// for the launch to reach that session, and for several providers it does
    /// not. Substring rather than equality because providers embed the id in
    /// a path (`pi --session ~/.pi/sessions/<id>.jsonl`) or a URL.
    ///
    /// # What this can and cannot establish
    ///
    /// It reads `self.args`, so it is only as true as the argv it is given. A
    /// spec built by a provider from its own values is structural and this is a
    /// faithful reading of it. A spec recovered by [`Self::from_command_line`]
    /// is not: the string it split may already have lost a word boundary, and
    /// no rule over the resulting words can recover one — `pi --session /tmp/Pi
    /// Home/sessions/<id>.jsonl` re-renders to itself, so the corruption is
    /// invisible from here. That is why Pi-Agent, the one provider whose resume
    /// form interpolates a path, builds its argv directly instead. A provider
    /// that interpolates any value that could contain whitespace must do the
    /// same; `launch_spec_test` pins that a value needing quotes never yields a
    /// *false* targeting claim, which is the failure that matters.
    ///
    /// An empty `session_id` never targets. Every argument contains the empty
    /// string, so the plain reading would have reported every provider in the
    /// registry as pointed at a session that does not exist.
    pub fn targeting_session(mut self, session_id: &str) -> Self {
        self.targets = (!session_id.is_empty()
            && self.args.iter().any(|arg| arg.contains(session_id)))
        .then(|| session_id.to_string());
        self
    }

    /// Whether launching this resumes the intended session.
    pub fn targeting(&self) -> SessionTargeting {
        match self.targets {
            Some(_) => SessionTargeting::ById,
            None => SessionTargeting::NotTargeted,
        }
    }

    /// Recover a spec from a rendered command line.
    ///
    /// The fallback for providers that only know how to produce a string.
    /// Quote-aware, so `open "https://example.com/c/x"` splits into two words
    /// rather than three. Returns `None` for input that is not splittable
    /// (unbalanced quotes) or that has no program word.
    pub fn from_command_line(line: &str) -> Option<Self> {
        let mut words = shlex::split(line)?.into_iter();
        let program = words.next()?;
        Some(Self::new(program, words))
    }

    /// Set the working directory.
    pub fn in_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Add an environment override.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Append user-supplied agent flags.
    ///
    /// Appended rather than merged: the resume arguments identify *which*
    /// session, and every agent here accepts its own options after them.
    ///
    /// Fails if an incoming flag is one the spec already sets, because the
    /// resulting command would be ambiguous at best and would target the wrong
    /// session at worst. Comparison is on the flag, so `--model=x` and
    /// `--model x` collide as they should.
    pub fn try_passthrough<I, S>(mut self, extra: I) -> Result<Self, LaunchError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let existing: Vec<&str> = self.args.iter().map(|arg| flag_name(arg)).collect();
        let mut appended = Vec::new();
        for arg in extra {
            let arg = arg.into();
            let name = flag_name(&arg);
            if !name.is_empty()
                && name.starts_with('-')
                && existing.contains(&name)
            {
                return Err(LaunchError::ConflictingFlag {
                    flag: name.to_string(),
                });
            }
            appended.push(arg);
        }
        self.args.extend(appended);
        Ok(self)
    }

    /// Shell-quoted rendering, for showing the user what will run.
    ///
    /// Display only. Nothing executes this — [`LaunchSpec::command`] bypasses
    /// the shell entirely — so a quoting bug here is a cosmetic defect rather
    /// than an injection.
    pub fn display(&self) -> String {
        let mut words = Vec::with_capacity(self.args.len() + 1);
        words.push(self.program.clone());
        words.extend(self.args.iter().cloned());
        shlex::try_join(words.iter().map(String::as_str))
            .unwrap_or_else(|_| words.join(" "))
    }

    /// The process to spawn.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    /// Whether the program can be found on `PATH`.
    ///
    /// Checked before launching so that a missing agent is reported as a
    /// missing agent, rather than as a failed conversion.
    pub fn program_path(&self) -> Option<PathBuf> {
        if Path::new(&self.program).is_absolute() {
            return Path::new(&self.program).exists().then(|| self.program.clone().into());
        }
        which::which(&self.program).ok()
    }
}

/// Why a launch could not be prepared.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error(
        "`{flag}` is already set by the resume command; passing it again would \
         target a different session than the one just written"
    )]
    ConflictingFlag { flag: String },
}

/// The flag part of an argument: `--model=opus` and `--model` both yield
/// `--model`; a bare value yields itself.
fn flag_name(arg: &str) -> &str {
    match arg.split_once('=') {
        Some((name, _)) if name.starts_with('-') => name,
        _ => arg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targeting_is_detected_not_assumed() {
        let targeted = LaunchSpec::from_command_line("claude --resume abc123")
            .expect("split")
            .targeting_session("abc123");
        assert_eq!(targeted.targeting(), SessionTargeting::ById);

        // Cursor's real resume form. The conversion still wrote a file; the
        // editor just will not open it.
        let untargeted = LaunchSpec::from_command_line("cursor .")
            .expect("split")
            .targeting_session("abc123");
        assert_eq!(untargeted.targeting(), SessionTargeting::NotTargeted);

        // Embedded in a path, as pi-agent does it.
        let in_path = LaunchSpec::from_command_line("pi --session /home/u/sessions/abc123.jsonl")
            .expect("split")
            .targeting_session("abc123");
        assert_eq!(in_path.targeting(), SessionTargeting::ById);
    }

    #[test]
    fn round_trips_through_a_command_line() {
        let spec = LaunchSpec::from_command_line("claude --resume abc123").expect("split");
        assert_eq!(spec.program, "claude");
        assert_eq!(spec.args, ["--resume", "abc123"]);
        assert_eq!(spec.display(), "claude --resume abc123");
    }

    #[test]
    fn quoted_arguments_stay_one_word() {
        let spec = LaunchSpec::from_command_line(
            r#"amp threads continue --execute "Continue from @s1""#,
        )
        .expect("split");
        assert_eq!(
            spec.args,
            ["threads", "continue", "--execute", "Continue from @s1"],
            "a quoted argument that splits into three words launches the wrong thing"
        );
    }

    #[test]
    fn unsplittable_input_is_refused_rather_than_guessed() {
        assert!(LaunchSpec::from_command_line(r#"claude --resume "unbalanced"#).is_none());
        assert!(LaunchSpec::from_command_line("   ").is_none());
    }

    #[test]
    fn passthrough_appends_user_flags() {
        let spec = LaunchSpec::new("claude", ["--resume".into(), "abc".into()])
            .try_passthrough(["--model", "opus", "--permission-mode", "plan"])
            .expect("no conflict");
        assert_eq!(
            spec.display(),
            "claude --resume abc --model opus --permission-mode plan"
        );
    }

    #[test]
    fn passthrough_refuses_to_retarget_the_session() {
        let error = LaunchSpec::new("claude", ["--resume".into(), "abc".into()])
            .try_passthrough(["--resume", "some-other-session"])
            .expect_err("must not silently start a different session");
        assert_eq!(
            error,
            LaunchError::ConflictingFlag {
                flag: "--resume".into()
            }
        );
    }

    #[test]
    fn conflict_detection_sees_through_equals_form() {
        let spec = LaunchSpec::new("agy", ["--conversation=abc".into()]);
        assert!(spec.clone().try_passthrough(["--conversation", "other"]).is_err());
        assert!(spec.try_passthrough(["--model", "x"]).is_ok());
    }

    #[test]
    fn display_quotes_what_needs_quoting() {
        let spec = LaunchSpec::new(
            "amp",
            ["--execute".into(), "Continue from @s1".into()],
        );
        // Single-quoted: inside single quotes `@`, `$` and the rest are inert,
        // so this is the form that survives being pasted into a shell.
        assert_eq!(spec.display(), "amp --execute 'Continue from @s1'");
        // And the round trip holds, which is the property that matters: what
        // is shown parses back to what will run.
        let reparsed = LaunchSpec::from_command_line(&spec.display()).expect("split");
        assert_eq!(reparsed.args, spec.args);
    }

    #[test]
    fn command_carries_cwd_and_env() {
        let spec = LaunchSpec::new("codex", ["resume".into(), "x".into()])
            .in_dir("/work")
            .with_env("CODEX_HOME", "/tmp/home");
        let command = spec.command();
        assert_eq!(command.get_current_dir(), Some(Path::new("/work")));
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(envs.len(), 1);
    }
}
