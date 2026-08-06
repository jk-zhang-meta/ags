#![forbid(unsafe_code)]

//! casr — Cross Agent Session Resumer.
//!
//! CLI entry point: parses arguments, dispatches subcommands, renders output.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{Local, Utc};
use clap::Parser;
use colored::Colorize;
use rayon::prelude::*;
use rich_rust::prelude::{Cell, Column, Console, JustifyMethod, Row, Style, Table};
use tracing_subscriber::EnvFilter;

use casr::budget::ContextBudget;
use casr::discovery::ProviderRegistry;
use casr::ir::Fidelity;
use casr::launch::{
    LaunchSpec, SessionTargeting,
};
use casr::pipeline::{ConversionPipeline, ConversionResult, ConvertOptions};
use casr::providers::{Provider, SessionListing, read_dir_reporting, walk_entry_reporting};
use casr::responses::{
    self, ErrorEnvelope, InfoResponse, ListEnvelope, ListItem, ProviderInfo, ResumeSuccess,
    SkippedSession,
};

/// Maximum characters per turn snippet in `info --peek` output.
const PEEK_SNIPPET_MAX_CHARS: usize = 200;

/// Cross Agent Session Resumer — resume AI coding sessions across providers.
///
/// Convert sessions between Claude Code, Codex, Gemini CLI, Antigravity CLI, Cursor, Cline, Aider, Amp, OpenCode, and
/// ChatGPT so you can pick up where you left off with a different agent.
#[derive(Parser, Debug)]
#[command(
    name = "casr",
    version = long_version(),
    about,
    long_about = None,
)]
struct Cli {
    /// Show detailed conversion progress.
    #[arg(long, global = true)]
    verbose: bool,

    /// Show everything including per-message parsing details.
    #[arg(long, global = true)]
    trace: bool,

    /// Output as JSON for machine consumption.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Manage encrypted, portable session checkpoints.
    #[command(trailing_var_arg = true, disable_help_flag = true)]
    Checkpoint {
        /// Arguments passed unchanged to the AGS checkpoint runtime.
        #[arg(value_name = "AGS_ARGS", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<OsString>,
    },

    /// Register a restored rollout in Codex's local thread index.
    #[command(hide = true)]
    CheckpointRegisterCodex {
        session_id: String,
        rollout_path: PathBuf,
        cwd: PathBuf,
    },

    /// Print an embedded checkpoint installer asset.
    #[command(hide = true)]
    CheckpointAsset { name: String },

    /// Convert and resume a session from another provider.
    ///
    /// `--launch` and `--launch-dry-run` are mutually exclusive, and
    /// `--launch-anyway` / `-- <agent flags>` only mean something alongside
    /// one of them.
    #[command(group(clap::ArgGroup::new("launching").args(["launch", "launch_dry_run"])))]
    Resume {
        /// Target provider alias (cc, cod, gmi, agy, cur, cln, aid, amp, opc, gpt).
        target: String,
        /// Session ID to convert.
        session_id: String,

        /// Show what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing session in target if it exists.
        #[arg(long)]
        force: bool,

        /// Explicitly specify source provider alias or session file path.
        #[arg(long)]
        source: Option<String>,

        /// Add context messages to help the target agent understand the conversion.
        #[arg(long)]
        enrich: bool,

        /// Cap the transferred history at roughly this many tokens (0 = no cap).
        /// Off unless you pass it: with neither --max-context-tokens nor
        /// --max-tool-output the whole session crosses untrimmed. Applies to
        /// cross-provider conversions on both tracks; the oldest turns are
        /// dropped first and the most recent history is kept. The flat track
        /// also pins the original task message; the structured track keeps a
        /// plain suffix and reports everything it dropped as a loss.
        #[arg(long)]
        max_context_tokens: Option<usize>,

        /// Truncate each tool result/observation to this many characters
        /// (0 = no cap). Off unless you pass it. Tool output is usually the bulk
        /// of a long session, so this often removes the need to drop any turn at
        /// all.
        #[arg(long)]
        max_tool_output: Option<usize>,

        /// Drop the source agent's reasoning traces. Off unless you pass it,
        /// and never implied by the two caps above: a limit on tool output is
        /// not a request to delete reasoning. Worth passing for a cross-agent
        /// handoff, where the target cannot replay another agent's hidden
        /// reasoning anyway and it is the cheapest thing to give up.
        #[arg(long)]
        drop_reasoning: bool,

        /// Accepted, and does nothing: keeping reasoning is the default. It used
        /// to be the opt-in half of an inverted pair, so existing command lines
        /// still work and still get what they asked for. To drop reasoning, say
        /// so with --drop-reasoning.
        #[arg(long, conflicts_with = "drop_reasoning")]
        keep_reasoning: bool,

        /// Start the target agent on the converted session instead of printing
        /// a command to paste.
        ///
        /// On Unix this process is replaced by the agent, so it inherits the
        /// terminal and there is no casr left waiting behind it.
        ///
        /// Aider and Cursor have no way to be pointed at a specific session.
        /// For those the agent is started plain, with a notice naming the file
        /// that was written, because the converted session will not be the one
        /// it opens.
        #[arg(long, conflicts_with = "dry_run")]
        launch: bool,

        /// Convert, then print the exact command `--launch` would run and stop.
        ///
        /// The session is still written; only the agent is not started.
        #[arg(long, conflicts_with = "dry_run")]
        launch_dry_run: bool,

        /// Launch even when the conversion could not carry part of the
        /// conversation across.
        #[arg(long, requires = "launching")]
        launch_anyway: bool,

        /// Do not consult the session store: read the session named here, write
        /// where told, and record nothing.
        ///
        /// The store exists so that converting back to a provider you started
        /// from can read the original session instead of a degraded copy of it.
        /// That means it may read a session other than the one named — which is
        /// always reported, and which this flag turns off. Also the escape hatch
        /// for a read-only or shared home: nothing here writes outside the
        /// target provider's own session directory.
        #[arg(long)]
        no_store: bool,

        /// Flags for the target agent, appended after the resume arguments.
        ///
        /// Refused if one of them is a flag the resume command already sets,
        /// since that would start a different session than the one just
        /// written.
        #[arg(last = true, value_name = "AGENT_ARGS", requires = "launching")]
        agent_args: Vec<String>,
    },

    /// List all discoverable sessions across installed providers.
    List {
        /// Filter by provider slug.
        #[arg(long)]
        provider: Option<String>,

        /// Filter by workspace path.
        #[arg(long)]
        workspace: Option<String>,

        /// Maximum sessions to show per provider.
        #[arg(long, default_value = "10")]
        limit: usize,

        /// Sort field (date, messages, provider).
        #[arg(long, default_value = "date")]
        sort: String,

        /// Enrich output with filesystem-derived data (e.g. repo_name from git root).
        #[arg(long)]
        enrich_fs: bool,
    },

    /// Show details for a specific session.
    Info {
        /// Session ID, or the path to a session file.
        ///
        /// The two are told apart by the filesystem: the argument is a path
        /// when it names a file that exists (or, for the providers whose
        /// session paths are `<db-file>/<id>`, when its parent does), and a
        /// session ID otherwise. Nothing syntactic can separate them — a Codex
        /// session ID is itself `2026/07/27/rollout-…`, so a rule about slashes
        /// or dots would swallow it. Use `./name.jsonl` for a file in the
        /// current directory whose name would otherwise read as an ID.
        session: String,

        /// Enrich output with filesystem-derived data (e.g. repo_name from git root).
        #[arg(long)]
        enrich_fs: bool,

        /// Disambiguate when the same session ID exists in multiple providers:
        /// a provider alias/slug (e.g. `opc`, `cc`) or a direct session file path.
        #[arg(long)]
        source: Option<String>,

        /// Force the reader instead of detecting one: a provider slug or alias
        /// (e.g. `codex`, `claude-code`, `cc`).
        ///
        /// Most useful alongside a path argument, where detection has only the
        /// file's own shape to go on. With a session ID it also decides which
        /// provider is asked to find it. The slug it resolves to is what
        /// `--json` reports as `detected_format`.
        #[arg(long)]
        from: Option<String>,

        /// Append a Transcript Tail section showing the last few turns of the
        /// session (the most recent turns help you recognize a session).
        #[arg(long)]
        peek: bool,

        /// Number of trailing turns to show (implies `--peek`; default 5).
        #[arg(long)]
        peek_lines: Option<usize>,
    },

    /// List detected providers and their installation status.
    Providers,

    /// Generate shell completions.
    Completions {
        /// Shell to generate completions for (bash, zsh, fish).
        shell: String,
    },
}

/// Build the long version string with embedded build metadata.
///
/// vergen-gix always emits these env vars (uses placeholders when values are
/// unavailable), so `env!()` is safe here.
fn long_version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("VERGEN_GIT_SHA"),
        " ",
        env!("VERGEN_BUILD_TIMESTAMP"),
        " ",
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
        ")",
    )
}

/// Initialize the tracing subscriber based on CLI flags.
///
/// JSON mode disables tracing so stdout/stderr each remain one parseable
/// envelope. Otherwise priority is `--trace` > `--verbose` > `RUST_LOG` >
/// default (warn).
fn init_tracing(cli: &Cli) {
    let filter = if cli.json {
        EnvFilter::new("off")
    } else if cli.trace {
        EnvFilter::new("casr=trace")
    } else if cli.verbose {
        EnvFilter::new("casr=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();
}

/// Rewrite ergonomic shorthand target flags into canonical resume commands.
///
/// Supports:
/// - `casr -cc <session-id> ...`
/// - `casr -cod <session-id> ...`
/// - `casr -gmi <session-id> ...`
/// - `casr -agy <session-id> ...`
///
/// Rewritten form:
/// `casr [global-options] resume <target> <session-id> ...`
fn rewrite_shorthand_resume_args(args: Vec<OsString>) -> Vec<OsString> {
    if args.len() < 2 {
        return args;
    }

    let mut shorthand_idx: Option<usize> = None;
    let mut target_alias: Option<&'static str> = None;

    // Only scan option-like tokens before the first positional token.
    // This preserves regular subcommand behavior (e.g., `casr list`).
    for (idx, arg) in args.iter().enumerate().skip(1) {
        let raw = arg.to_string_lossy();
        if raw == "--" {
            break;
        }
        if !raw.starts_with('-') {
            break;
        }

        let alias = match raw.as_ref() {
            "-cc" => Some("cc"),
            "-cod" => Some("cod"),
            "-gmi" => Some("gmi"),
            "-agy" => Some("agy"),
            _ => None,
        };

        if let Some(a) = alias {
            shorthand_idx = Some(idx);
            target_alias = Some(a);
            break;
        }
    }

    let (idx, alias) = match (shorthand_idx, target_alias) {
        (Some(i), Some(a)) => (i, a),
        _ => return args,
    };

    let mut rewritten = Vec::with_capacity(args.len() + 1);
    rewritten.push(args[0].clone());

    // Preserve any global options before the shorthand flag.
    rewritten.extend(args.iter().take(idx).skip(1).cloned());

    rewritten.push(OsString::from("resume"));
    rewritten.push(OsString::from(alias));

    // Preserve the remaining args after shorthand (session id + options).
    rewritten.extend(args.into_iter().skip(idx + 1));

    rewritten
}

fn main() -> ExitCode {
    let argv = rewrite_shorthand_resume_args(std::env::args_os().collect());
    if argv.get(1).is_some_and(|arg| arg == "checkpoint") {
        return cmd_checkpoint(&argv[2..]);
    }
    let cli = Cli::parse_from(argv);
    init_tracing(&cli);

    let result = match cli.command {
        Command::Checkpoint { args } => return cmd_checkpoint(&args),
        Command::CheckpointRegisterCodex {
            session_id,
            rollout_path,
            cwd,
        } => casr::providers::codex::register_restored_thread(&session_id, &rollout_path, &cwd)
            .map(|()| ExitCode::SUCCESS),
        Command::CheckpointAsset { name } => casr::checkpoint_runtime::asset(&name).map_or_else(
            || Err(anyhow::anyhow!("unknown checkpoint asset: {name}")),
            |asset| {
                print!("{asset}");
                Ok(ExitCode::SUCCESS)
            },
        ),
        Command::Resume {
            target,
            session_id,
            dry_run,
            force,
            source,
            enrich,
            max_context_tokens,
            max_tool_output,
            drop_reasoning,
            // Accepted for compatibility and deliberately unread: it names the
            // default, and `conflicts_with` has already refused the one command
            // line where reading it could change an answer.
            keep_reasoning: _,
            launch,
            launch_dry_run,
            launch_anyway,
            no_store,
            agent_args,
        } => cmd_resume(
            &target,
            &session_id,
            dry_run,
            force,
            source,
            enrich,
            max_context_tokens,
            max_tool_output,
            drop_reasoning,
            no_store,
            cli.json,
            LaunchRequest {
                launch,
                dry_run: launch_dry_run,
                anyway: launch_anyway,
                agent_args,
            },
        ),
        Command::List {
            provider,
            workspace,
            limit,
            sort,
            enrich_fs,
        } => cmd_list(
            provider.as_deref(),
            workspace.as_deref(),
            limit,
            &sort,
            cli.json,
            enrich_fs,
        )
        .map(|()| ExitCode::SUCCESS),
        Command::Info {
            session,
            enrich_fs,
            source,
            from,
            peek,
            peek_lines,
        } => cmd_info(
            &session, cli.json, enrich_fs, source, from, peek, peek_lines,
        )
        .map(|()| ExitCode::SUCCESS),
        Command::Providers => cmd_providers(cli.json).map(|()| ExitCode::SUCCESS),
        Command::Completions { shell } => cmd_completions(&shell).map(|()| ExitCode::SUCCESS),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            if cli.json {
                let envelope = ErrorEnvelope::new(error_type_name(&e), format!("{e}"));
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&envelope).unwrap_or_default()
                );
            } else {
                eprintln!("{} {e}", "Error:".red().bold());
            }
            ExitCode::FAILURE
        }
    }
}

fn cmd_checkpoint(args: &[OsString]) -> ExitCode {
    match casr::checkpoint_runtime::run(args) {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("{} {error:#}", "Error:".red().bold());
            ExitCode::FAILURE
        }
    }
}

/// Extract a short error type name for JSON output.
fn error_type_name(e: &anyhow::Error) -> &'static str {
    if let Some(casr_err) = e.downcast_ref::<casr::error::CasrError>() {
        match casr_err {
            casr::error::CasrError::SessionNotFound { .. } => "SessionNotFound",
            casr::error::CasrError::AmbiguousSessionId { .. } => "AmbiguousSessionId",
            casr::error::CasrError::UnknownProviderAlias { .. } => "UnknownProviderAlias",
            casr::error::CasrError::ProviderUnavailable { .. } => "ProviderUnavailable",
            casr::error::CasrError::SessionReadError { .. } => "SessionReadError",
            casr::error::CasrError::SessionWriteError { .. } => "SessionWriteError",
            casr::error::CasrError::SessionConflict { .. } => "SessionConflict",
            casr::error::CasrError::ValidationError { .. } => "ValidationError",
            casr::error::CasrError::VerifyFailed { .. } => "VerifyFailed",
        }
    } else {
        "InternalError"
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// What the user asked the launcher to do, kept together so the four flags
/// travel as one argument rather than four more booleans.
struct LaunchRequest {
    launch: bool,
    dry_run: bool,
    anyway: bool,
    agent_args: Vec<String>,
}

impl LaunchRequest {
    /// Whether a launch was asked for at all, in either form.
    fn wanted(&self) -> bool {
        self.launch || self.dry_run
    }
}

/// The session store, unless `--no-store` said not to.
///
/// A store that will not open is a warning and a `None`, never an error: every
/// conversion this tool did before the store existed still works without one, so
/// a read-only home or a store written by a newer build must not take the tool
/// with it. The failure is printed rather than swallowed, because a silently
/// absent store would look exactly like a store that had nothing to say.
///
/// # Why a dry run will not create one
///
/// `Store::open` creates the store on first use, and `--dry-run` promises to
/// write nothing at all — a promise the store's own manifest broke, caught by
/// `trace_dry_run_omits_atomic_write`. Declining to create it costs a dry run no
/// information: a store that does not exist yet holds no records, so the only
/// thing it could have said is that it had never seen this conversation. An
/// existing store is opened and consulted in full, so `--dry-run` still reports
/// the source a real run would read.
fn open_store(no_store: bool, dry_run: bool) -> Option<casr::store::Store> {
    if no_store {
        return None;
    }
    if dry_run && !casr::store::default_root().is_ok_and(|root| root.join("store.json").is_file()) {
        return None;
    }
    match casr::store::Store::open() {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!(
                "{} the session store is unavailable, so this conversion reads exactly the \
                 session named and records nothing: {error}",
                "⚠".yellow()
            );
            None
        }
    }
}

/// Turn one of our record ids into a session the pipeline can resolve.
///
/// `ags resume cc <record-id>` is the point of having our own identifier: one id
/// for a conversation however many providers it has been through. No provider has
/// heard of it, so it is translated here, and the pair returned is `(session_id,
/// source_hint)` — the hint pins the provider so that a native session id that
/// happens to collide across providers cannot be resolved to the wrong one.
///
/// Anything that is not a record id is returned untouched, which is every
/// ordinary invocation.
///
/// # Why the substitution is reported
///
/// The third element is a sentence, present exactly when this function changed
/// the session that gets converted. Without it the redirection was invisible:
/// `print_source_selection` only speaks when the *store* reads something other
/// than what the pipeline was asked for, and by then the pipeline has been asked
/// for the redirected session, so it agrees and says nothing. The user typed one
/// identifier, some other provider's session was converted, and no line of
/// output connected the two.
///
/// That is also the honest answer to record ids and provider session ids sharing
/// a namespace. A record id is a fresh v4 UUID, so a provider session colliding
/// with one is not a case worth designing a `record:` prefix around — it would
/// break the documented `resume <record-id>` form to defend against a 2⁻¹²²
/// event. What was worth fixing is that when the lookup *does* redirect, for any
/// reason, it says so.
fn redirect_record_id(
    store: Option<&casr::store::Store>,
    target: &str,
    session_id: &str,
    source: Option<String>,
) -> (String, Option<String>, Option<String>) {
    let untouched = (session_id.to_string(), source, None);
    let Some(store) = store else {
        return untouched;
    };
    // A `--source` was given explicitly, so the user is naming a provider
    // session and not a record.
    if untouched.1.is_some() {
        return untouched;
    }
    let Ok(Some(record)) = store.get(session_id) else {
        return untouched;
    };
    let registry = ProviderRegistry::default_registry();
    let target_slug = registry
        .find_by_alias(target)
        .map(|provider| provider.slug().to_string())
        .unwrap_or_else(|| target.to_string());
    match casr::launch::session_named_by_record(&record, &target_slug) {
        Some(key) => (
            key.provider_session_id.clone(),
            Some(key.provider.clone()),
            Some(format!(
                "'{session_id}' is a session-store record id, not a provider session; it names \
                 the {} session {} in this conversation, and that is what was converted.",
                key.provider, key.provider_session_id
            )),
        ),
        None => untouched,
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_resume(
    target: &str,
    session_id: &str,
    dry_run: bool,
    force: bool,
    source: Option<String>,
    enrich: bool,
    max_context_tokens: Option<usize>,
    max_tool_output: Option<usize>,
    drop_reasoning: bool,
    no_store: bool,
    json_mode: bool,
    launch: LaunchRequest,
) -> anyhow::Result<ExitCode> {
    let registry = ProviderRegistry::default_registry();
    let store = open_store(no_store, dry_run);
    // The store may be holding the conversation under our own record id rather
    // than a provider's session id, in which case the positional argument names
    // no session any provider has heard of. Translated before the pipeline runs,
    // so that everything downstream sees an ordinary source session.
    let (session_id, source, redirection) =
        redirect_record_id(store.as_ref(), target, session_id, source);
    let pipeline = ConversionPipeline { registry, store };

    let opts = ConvertOptions {
        dry_run,
        force,
        verbose: false,
        enrich,
        source_hint: source,
        // Absent flags mean an absent budget. A converter graded on fidelity
        // does not trim a session nobody asked it to trim.
        budget: ContextBudget::requested(max_context_tokens, max_tool_output, drop_reasoning),
    };

    let result = pipeline.convert(target, &session_id, opts)?;

    // Everything the user should be told, in one list, whoever produced it. The
    // record-id redirection happens before the pipeline exists, so it has no
    // other way in.
    let warnings: Vec<String> = redirection
        .iter()
        .cloned()
        .chain(result.warnings.iter().cloned())
        .collect();

    // Resolved before anything is printed, because the JSON envelope carries
    // the command and the envelope is printed first. The error is held rather
    // than propagated for the same reason the missing-agent report is deferred:
    // a launch that cannot be prepared does not undo a conversion that wrote
    // files, and the written paths are the useful part of that output.
    let prepared: Option<(&dyn Provider, anyhow::Result<LaunchSpec>)> =
        launch.wanted().then(|| {
            let provider = pipeline
                .registry
                .find_by_alias(target)
                .expect("convert() already resolved this alias");
            let spec = prepare_launch(provider, &result, &launch);
            (provider, spec)
        });

    // Decided here rather than inside `run_launch`, because the envelope below
    // is printed first and used to say `ok: true` on stdout while the very same
    // invocation put an error envelope on stderr and exited non-zero. A consumer
    // reading stdout was told the run succeeded by the run that failed. Held
    // rather than propagated so that the written paths — the part worth having —
    // stay in the output.
    let launch_error: Option<String> = prepared.as_ref().and_then(|(provider, spec)| match spec {
        Err(error) => Some(format!("{error}")),
        Ok(spec) => launch_blocker(*provider, &result, spec, &launch, json_mode),
    });

    if json_mode {
        let spec = prepared.as_ref().and_then(|(_, spec)| spec.as_ref().ok());
        let response = ResumeSuccess {
            ok: launch_error.is_none(),
            source_provider: result.source_provider.clone(),
            target_provider: result.target_provider.clone(),
            source_session_id: result.canonical_session.session_id.clone(),
            target_session_id: result.written.as_ref().map(|w| w.session_id.clone()),
            written_paths: result
                .written
                .as_ref()
                .map(|w| w.paths.iter().map(|p| p.display().to_string()).collect()),
            resume_command: result.written.as_ref().map(|w| w.resume_command.clone()),
            dry_run: result.written.is_none(),
            fidelity: result.fidelity,
            verified_fidelity: result.verified_fidelity,
            losses: result.losses.clone(),
            launch_command: spec.map(LaunchSpec::display),
            launch_targets_session: spec.map(|s| s.targeting() == SessionTargeting::ById),
            launch_error: launch_error.clone(),
            warnings: warnings.clone(),
            source_selection: result
                .source
                .as_ref()
                .and_then(responses::SourceSelectionJson::of),
        };
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if let Some(ref written) = result.written {
        println!(
            "{} Converted {} session to {}",
            "✓".green().bold(),
            result.source_provider.cyan(),
            result.target_provider.cyan()
        );
        println!(
            "  {} → {}",
            "Source".dimmed(),
            result.canonical_session.session_id
        );
        print_source_selection(&result);
        println!("  {} → {}", "Target".dimmed(), written.session_id);
        println!(
            "  {} → {}",
            "Messages".dimmed(),
            result.canonical_session.messages.len()
        );
        for path in &written.paths {
            println!("  {} → {}", "Written".dimmed(), path.display());
        }
        for warning in &warnings {
            println!("  {} {warning}", "⚠".yellow());
        }
        println!();
        println!(
            "  {} {}",
            "Resume:".green().bold(),
            written.resume_command.bold()
        );
    } else {
        // Dry run.
        println!(
            "{} Would convert {} session to {}",
            "⊘".cyan().bold(),
            result.source_provider.cyan(),
            result.target_provider.cyan()
        );
        println!(
            "  {} → {} messages",
            "Messages".dimmed(),
            result.canonical_session.messages.len()
        );
        print_source_selection(&result);
        for warning in &warnings {
            println!("  {} {warning}", "⚠".yellow());
        }
    }

    let Some((provider, spec)) = prepared else {
        return Ok(ExitCode::SUCCESS);
    };
    // The envelope already carries this and already says `ok: false`. It is
    // still raised as an error so that the exit code is non-zero and the reason
    // reaches stderr, where a human reading a terminal will find it.
    if let Some(blocker) = launch_error {
        anyhow::bail!("{blocker}");
    }
    run_launch(
        provider,
        &result,
        spec.expect("a spec that failed to resolve is a launch_error"),
        &launch,
        json_mode,
    )
}

/// Say so when the store read a session other than the one that was named.
///
/// Printed only then. When the store agrees with the user there is nothing to
/// tell them, and a line that appears on every conversion is a line nobody reads
/// — which is the same as not printing the one that matters.
fn print_source_selection(result: &ConversionResult) {
    if let Some(selection) = result.source.as_ref().filter(|s| s.overrode()) {
        println!("  {} {}", "⤺".cyan(), selection.line());
    }
}

/// The command a launch would run, resolved without running it.
///
/// Separate from [`run_launch`] because the JSON envelope reports the command
/// and is printed before the launch happens, so the spec has to exist first.
fn prepare_launch(
    provider: &dyn Provider,
    result: &ConversionResult,
    launch: &LaunchRequest,
) -> anyhow::Result<LaunchSpec> {
    // `--launch` and `--launch-dry-run` both conflict with `--dry-run`.
    let written = result
        .written
        .as_ref()
        .expect("a launched conversion is never a dry run");

    provider
        .launch_spec(&written.session_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} cannot be started: its resume form {:?} is not a command line",
                provider.name(),
                written.resume_command
            )
        })?
        .try_passthrough(launch.agent_args.iter().cloned())
        .map_err(Into::into)
}

/// Why this launch is not going to happen, decided before anything is printed.
///
/// `None` means it will. Everything here is a pure function of the conversion
/// and the resolved spec, which is the point: the JSON envelope is printed
/// first and has to be able to say `ok: false`, and it can only do that if the
/// decision is available before the printing rather than made afterwards inside
/// [`run_launch`]. One invocation, one machine-readable object, and an `ok`
/// that means what the exit code means.
///
/// Whether the agent is on `PATH` is included; whether it *starts* is not,
/// because that is not knowable without starting it.
fn launch_blocker(
    provider: &dyn Provider,
    result: &ConversionResult,
    spec: &LaunchSpec,
    launch: &LaunchRequest,
    json_mode: bool,
) -> Option<String> {
    if json_mode && !launch.dry_run {
        return Some(
            "--json cannot be combined with an interactive --launch because process startup \
             cannot be represented as a completed JSON result; use --launch-dry-run to inspect \
             the managed launch"
                .to_string(),
        );
    }

    // A hole in the conversation blocks rather than warns: the agent would
    // start missing history and neither it nor the user would be told. Every
    // grade above this one is a degraded *rendering* of a whole conversation,
    // which is survivable — and since `Fidelity` is ordered best-first, `>=`
    // is "at least this bad" rather than "at least this good".
    //
    // `effective_fidelity` rather than `fidelity`, and the difference is not
    // cosmetic: `fidelity` is the writer's own claim, and a writer that
    // under-reports is exactly the case a refusal exists to survive. When the
    // read-back verifier independently derived a worse grade, the session it
    // examined is the session about to be resumed.
    let grade = result.effective_fidelity();
    if grade >= Fidelity::HistoryIncomplete && !launch.anyway {
        let mut refusal = format!("refusing to launch — {}.", grade.describe());
        // The grade names the category; the losses name the capsules, the
        // counts, and the vendor that sealed them, which is what the user can
        // act on. Only the losses that forced this grade are worth printing
        // here — a dropped-reasoning note alongside a missing-history refusal
        // reads as though the two were comparable.
        for loss in result
            .losses
            .iter()
            .filter(|loss| loss.grade >= Fidelity::HistoryIncomplete)
        {
            refusal.push(' ');
            refusal.push_str(&loss.note);
        }
        // The writer's loss list is what those notes come from, so a refusal
        // driven by the verifier alone would otherwise arrive with no reason
        // attached — and the disagreement is itself the finding.
        if let Some(disagreement) = result.fidelity_disagreement() {
            refusal.push(' ');
            refusal.push_str(&disagreement);
        }
        refusal.push_str(" Pass --launch-anyway to start it regardless.");
        return Some(refusal);
    }

    // A dry run is allowed to describe a launch of an agent that is not
    // installed yet — checking exactly that is one of the things it is for.
    if !launch.dry_run && spec.program_path().is_none() {
        return Some(format!(
            "{} is not installed: `{}` is not on PATH. The session was converted and \
             written; install the agent, then rerun this AGS command. No unmanaged agent \
             was started",
            provider.name(),
            spec.program
        ));
    }
    if !launch.dry_run && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
        return Some(
            "an interactive AGS launch requires a real terminal on stdin and stdout. The session \
             was converted and written; no agent was started."
                .to_string(),
        );
    }
    None
}

/// Start the target agent on the session the conversion just wrote.
///
/// Every reason not to start it has already been decided by
/// [`launch_blocker`], so reaching this function means the launch is happening
/// (or being described, under `--launch-dry-run`).
fn run_launch(
    provider: &dyn Provider,
    result: &ConversionResult,
    spec: LaunchSpec,
    launch: &LaunchRequest,
    json_mode: bool,
) -> anyhow::Result<ExitCode> {
    let written = result
        .written
        .as_ref()
        .expect("a launched conversion is never a dry run");

    if spec.targeting() == SessionTargeting::NotTargeted {
        launch_line(
            json_mode,
            &format!(
                "{} {} has no way to be pointed at a specific session, so it will start \
                 without resuming the converted one.",
                "⚠".yellow(),
                provider.name()
            ),
        );
        for path in &written.paths {
            launch_line(
                json_mode,
                &format!("  Converted session: {}", path.display()),
            );
        }
    }

    if launch.dry_run {
        launch_line(json_mode, &spec.display());
        return Ok(ExitCode::SUCCESS);
    }

    launch_agent(&spec)
}

/// Emit a launch-time line, unless the JSON envelope already carries it.
///
/// Under `--json` the same information is in `launch_command` and
/// `launch_targets_session`, so printing it again would either corrupt stdout
/// for the callers who asked for parseable output or duplicate it on stderr.
fn launch_line(json_mode: bool, line: &str) {
    if !json_mode {
        println!("{line}");
    }
}

/// Replace this process with the Agent, in the very shell AGS was started from.
///
/// AGS deliberately owns no terminal of its own: the Agent inherits this PTY
/// unchanged, so nothing re-emulates the terminal underneath it.
#[cfg(unix)]
fn launch_agent(spec: &LaunchSpec) -> anyhow::Result<ExitCode> {
    use std::os::unix::process::CommandExt;

    // exec discards anything still buffered, including the written paths.
    let _ = std::io::stdout().flush();
    let error = spec.command().exec();
    Err(anyhow::anyhow!(
        "failed to start `{}`: {error}",
        spec.program
    ))
}

#[cfg(not(unix))]
fn launch_agent(spec: &LaunchSpec) -> anyhow::Result<ExitCode> {
    let _ = std::io::stdout().flush();
    let status = spec
        .command()
        .status()
        .map_err(|error| anyhow::anyhow!("failed to start `{}`: {error}", spec.program))?;
    // A signal death carries no code; reporting it as success would tell a
    // script the session ended cleanly when it was killed.
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

fn cmd_list(
    provider_filter: Option<&str>,
    workspace_filter: Option<&str>,
    limit: usize,
    sort: &str,
    json_mode: bool,
    enrich_fs: bool,
) -> anyhow::Result<()> {
    let registry = ProviderRegistry::default_registry();
    let installed = registry.installed_providers();
    let provider_filter_slug = provider_filter
        .and_then(|filter| registry.find_by_alias(filter).map(|p| p.slug().to_string()))
        .or_else(|| provider_filter.map(|filter| filter.to_ascii_lowercase()));

    #[derive(Debug)]
    struct SessionSummary {
        session_id: String,
        provider: String,
        title: Option<String>,
        native_name: Option<String>,
        messages: usize,
        workspace: Option<PathBuf>,
        started_at: Option<i64>,
        last_active_at: Option<i64>,
        file_size_bytes: u64,
        unique_user_messages: usize,
        avg_agent_response_chars: f64,
        /// `None` where nothing could count them; see `ListItem::tool_uses`.
        tool_uses: Option<usize>,
        path: PathBuf,
    }

    impl SessionSummary {
        fn recency_value(&self) -> i64 {
            self.last_active_at.or(self.started_at).unwrap_or(0)
        }

        fn file_size_kb_rounded(&self) -> u64 {
            ((self.file_size_bytes as f64) / 1024.0).round() as u64
        }

        fn file_size_display(&self) -> String {
            format_with_commas(self.file_size_kb_rounded())
        }

        fn avg_agent_chars_rounded(&self) -> u64 {
            self.avg_agent_response_chars.round() as u64
        }

        fn avg_agent_chars_display(&self) -> String {
            format_with_commas(self.avg_agent_chars_rounded())
        }

        fn started_at_display(&self) -> String {
            self.started_at
                .and_then(chrono::DateTime::<Utc>::from_timestamp_millis)
                .map(|dt| {
                    dt.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "-".to_string())
        }

        fn last_active_display(&self, now_millis: i64) -> String {
            self.last_active_at
                .map(|timestamp| format_relative_age(timestamp, now_millis))
                .unwrap_or_else(|| "-".to_string())
        }

        fn to_list_item(&self, enrich_fs: bool) -> ListItem {
            let (workspace_name, workspace_name_source) =
                responses::workspace_name_from_path(self.workspace.as_ref());
            let repo_name = if enrich_fs {
                self.workspace
                    .as_ref()
                    .and_then(|ws| casr::discovery::repo_name_from_path(ws))
            } else {
                None
            };
            ListItem {
                schema_version: responses::SCHEMA_VERSION,
                session_id: self.session_id.clone(),
                provider: self.provider.clone(),
                title: self.title.clone(),
                native_name: self.native_name.clone(),
                messages: self.messages,
                workspace: self.workspace.as_ref().map(|w| w.display().to_string()),
                started_at: self.started_at,
                last_active_at: self.last_active_at,
                file_size_bytes: self.file_size_bytes,
                file_size_kb: self.file_size_kb_rounded(),
                unique_user_messages: self.unique_user_messages,
                avg_agent_response_chars: self.avg_agent_response_chars,
                avg_agent_response_chars_rounded: self.avg_agent_chars_rounded(),
                tool_uses: self.tool_uses,
                path: self.path.display().to_string(),
                workspace_name,
                workspace_name_source,
                repo_name,
            }
        }
    }

    fn expand_tilde_path(value: &str) -> PathBuf {
        if let Some(rest) = value.strip_prefix("~/")
            && let Some(home) = dirs::home_dir()
        {
            home.join(rest)
        } else {
            PathBuf::from(value)
        }
    }

    fn system_time_to_epoch_millis(time: std::time::SystemTime) -> Option<i64> {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|dur| i64::try_from(dur.as_millis()).ok())
    }

    fn file_mtime_millis(path: &Path) -> i64 {
        path.metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_to_epoch_millis)
            .unwrap_or(0)
    }

    fn file_last_activity_millis(path: &Path) -> Option<i64> {
        path.metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_to_epoch_millis)
    }

    fn session_activity_millis(
        session: &casr::model::CanonicalSession,
        path: &Path,
    ) -> Option<i64> {
        let conversation_activity = session
            .ended_at
            .or_else(|| {
                session
                    .messages
                    .iter()
                    .filter_map(|msg| msg.timestamp)
                    .max()
            })
            .or(session.started_at);
        let file_activity = file_last_activity_millis(path);
        match (conversation_activity, file_activity) {
            (Some(conversation), Some(file)) => Some(conversation.max(file)),
            (Some(conversation), None) => Some(conversation),
            (None, Some(file)) => Some(file),
            (None, None) => None,
        }
    }

    fn format_relative_age(timestamp_millis: i64, now_millis: i64) -> String {
        let (delta_millis, suffix) = if now_millis >= timestamp_millis {
            (now_millis.saturating_sub(timestamp_millis), "ago")
        } else {
            (timestamp_millis.saturating_sub(now_millis), "from now")
        };
        let total_seconds = u64::try_from(delta_millis / 1000).unwrap_or(0);
        let days = total_seconds / 86_400;
        let hours = (total_seconds % 86_400) / 3_600;
        let minutes = (total_seconds % 3_600) / 60;
        let seconds = total_seconds % 60;
        format!("{days}d {hours:02}h {minutes:02}m {seconds:02}s {suffix}")
    }

    fn format_with_commas(value: u64) -> String {
        let s = value.to_string();
        let mut out = String::with_capacity(s.len() + (s.len() / 3));
        for (i, ch) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out.chars().rev().collect()
    }

    /// Collapse a native name to a single line and clamp its display width.
    fn truncate_display_name(name: &str, max_len: usize) -> String {
        let collapsed: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if max_len == 0 || collapsed.chars().count() <= max_len {
            return collapsed;
        }
        let keep = max_len.saturating_sub(1);
        let truncated: String = collapsed.chars().take(keep).collect();
        format!("{truncated}…")
    }

    fn codex_tool_uses_from_file(path: &Path) -> usize {
        let Ok(file) = std::fs::File::open(path) else {
            return 0;
        };
        let reader = BufReader::new(file);
        let mut count: usize = 0;

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if entry.get("type").and_then(|v| v.as_str()) != Some("response_item") {
                continue;
            }
            let payload_type = entry
                .pointer("/payload/type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if matches!(payload_type, "function_call" | "custom_tool_call") {
                count = count.saturating_add(1);
            }
            if let Some(content) = entry.pointer("/payload/content").and_then(|v| v.as_array()) {
                count = count.saturating_add(
                    content
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                        })
                        .count(),
                );
            }
        }

        count
    }

    fn gemini_tool_uses_from_file(path: &Path) -> usize {
        let Ok(content) = std::fs::read_to_string(path) else {
            return 0;
        };
        let Ok(root) = serde_json::from_str::<serde_json::Value>(&content) else {
            return 0;
        };
        let mut count: usize = 0;
        if let Some(messages) = root.get("messages").and_then(|v| v.as_array()) {
            for msg in messages {
                if let Some(parts) = msg.get("content").and_then(|v| v.as_array()) {
                    count = count.saturating_add(
                        parts
                            .iter()
                            .filter(|part| {
                                part.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                            })
                            .count(),
                    );
                }
                if let Some(tool_calls) = msg.get("toolCalls").and_then(|v| v.as_array()) {
                    count = count.saturating_add(tool_calls.len());
                }
            }
        }
        count
    }

    fn claude_tool_uses_from_file(path: &Path) -> usize {
        let Ok(file) = std::fs::File::open(path) else {
            return 0;
        };
        let reader = BufReader::new(file);
        let mut count: usize = 0;

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if let Some(content) = entry.pointer("/message/content").and_then(|v| v.as_array()) {
                count = count.saturating_add(
                    content
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                        })
                        .count(),
                );
            }
        }

        count
    }

    fn factory_tool_uses_from_file(path: &Path) -> usize {
        let Ok(file) = std::fs::File::open(path) else {
            return 0;
        };
        let reader = BufReader::new(file);
        let mut count: usize = 0;

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            if let Some(content) = entry.pointer("/message/content").and_then(|v| v.as_array()) {
                count = count.saturating_add(
                    content
                        .iter()
                        .filter(|block| {
                            matches!(
                                block.get("type").and_then(|v| v.as_str()),
                                Some("tool_use")
                                    | Some("tool_call")
                                    | Some("function_call")
                                    | Some("custom_tool_call")
                            )
                        })
                        .count(),
                );
            }
            if let Some(tool_calls) = entry
                .pointer("/message/toolCalls")
                .and_then(|v| v.as_array())
            {
                count = count.saturating_add(tool_calls.len());
            }
        }

        count
    }

    /// Scan a source file for tool uses, for the providers that have a scanner.
    ///
    /// `None` means "there is no scanner for this provider", which is not the
    /// same claim as `Some(0)`. The arm used to be `_ => 0`, so the seventeen
    /// providers with no scanner reported zero tool uses for every session they
    /// have ever had. A count nothing produced is not a count.
    ///
    /// The wildcard stays, because this matches on a provider slug rather than
    /// on an enum and no exhaustiveness is available to enforce; what changed
    /// is that it now returns the absence rather than inventing a number.
    fn tool_uses_from_source_file(provider_slug: &str, path: &Path) -> Option<usize> {
        match provider_slug {
            "codex" => Some(codex_tool_uses_from_file(path)),
            "gemini" => Some(gemini_tool_uses_from_file(path)),
            "claude-code" => Some(claude_tool_uses_from_file(path)),
            "factory" => Some(factory_tool_uses_from_file(path)),
            _ => None,
        }
    }

    fn message_count_style(message_count: usize) -> Style {
        let style_str = if message_count >= 200 {
            "bold bright_cyan"
        } else if message_count >= 50 {
            "bold cyan"
        } else if message_count >= 10 {
            "bold blue"
        } else {
            "bold dim"
        };
        Style::parse(style_str).unwrap_or_default()
    }

    fn last_active_style(last_active_at: Option<i64>, now_millis: i64) -> Style {
        let Some(last_active_at) = last_active_at else {
            return Style::parse("dim").unwrap_or_default();
        };
        let age_seconds =
            u64::try_from(now_millis.saturating_sub(last_active_at).max(0) / 1000).unwrap_or(0);
        let style_str = if age_seconds < 3_600 {
            "bold bright_green"
        } else if age_seconds < 86_400 {
            "bold green"
        } else if age_seconds < 604_800 {
            "bold yellow"
        } else if age_seconds < 2_592_000 {
            "bold magenta"
        } else {
            "bold dim"
        };
        Style::parse(style_str).unwrap_or_default()
    }

    fn provider_display(provider: &str) -> &str {
        match provider {
            "claude-code" => "Claude Code",
            "codex" => "Codex",
            "gemini" => "Gemini",
            "cursor" => "Cursor",
            "cline" => "Cline",
            "aider" => "Aider",
            "amp" => "Amp",
            "opencode" => "OpenCode",
            "chatgpt" => "ChatGPT",
            "clawdbot" => "ClawdBot",
            "vibe" => "Vibe",
            "factory" => "Factory",
            "openclaw" => "OpenClaw",
            "pi-agent" => "Pi-Agent",
            "kiro" => "Kiro CLI",
            "grok" => "Grok Build",
            _ => provider,
        }
    }

    fn normalize_user_message_for_uniqueness(content: &str) -> Option<String> {
        let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    fn session_metrics(
        provider_slug: &str,
        session: &casr::model::CanonicalSession,
        path: &Path,
    ) -> (u64, usize, f64, Option<usize>) {
        let file_size_bytes = path.metadata().map(|meta| meta.len()).unwrap_or(0);

        let mut unique_user_messages: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut assistant_chars_total: usize = 0;
        let mut assistant_responses: usize = 0;
        let mut canonical_tool_uses: usize = 0;

        for msg in &session.messages {
            canonical_tool_uses = canonical_tool_uses.saturating_add(msg.tool_calls.len());

            if msg.role == casr::model::MessageRole::User
                && let Some(normalized) = normalize_user_message_for_uniqueness(&msg.content)
            {
                unique_user_messages.insert(normalized);
            }

            if msg.role == casr::model::MessageRole::Assistant {
                let char_count = msg.content.chars().count().saturating_add(
                    msg.tool_results
                        .iter()
                        .map(|result| result.content.chars().count())
                        .sum::<usize>(),
                );
                if char_count > 0 {
                    assistant_chars_total = assistant_chars_total.saturating_add(char_count);
                    assistant_responses = assistant_responses.saturating_add(1);
                }
            }
        }

        let avg_agent_response_chars = if assistant_responses > 0 {
            assistant_chars_total as f64 / assistant_responses as f64
        } else {
            0.0
        };

        // A non-zero canonical count is the count. A zero is ambiguous — most
        // flat readers never populate `tool_calls` at all — so it defers to the
        // provider's own scanner, and where there is no scanner the answer is
        // `None`: nothing here established that the session has no tool calls.
        let tool_uses = if canonical_tool_uses > 0 {
            Some(canonical_tool_uses)
        } else {
            tool_uses_from_source_file(provider_slug, path)
        };

        (
            file_size_bytes,
            unique_user_messages.len(),
            avg_agent_response_chars,
            tool_uses,
        )
    }

    fn build_summary(
        provider_slug: &str,
        path: PathBuf,
        session: casr::model::CanonicalSession,
    ) -> SessionSummary {
        let last_active_at = session_activity_millis(&session, &path);
        let (file_size_bytes, unique_user_messages, avg_agent_response_chars, tool_uses) =
            session_metrics(provider_slug, &session, &path);
        let native_name = casr::model::native_name_from_metadata(&session.metadata);

        SessionSummary {
            session_id: session.session_id,
            provider: provider_slug.to_string(),
            title: session.title,
            native_name,
            messages: session.messages.len(),
            workspace: session.workspace,
            started_at: session.started_at,
            last_active_at,
            file_size_bytes,
            unique_user_messages,
            avg_agent_response_chars,
            tool_uses,
            path,
        }
    }

    fn probe_limit_for_sort(limit: usize, sort: &str, workspace_scoped: bool) -> usize {
        if sort == "date" {
            // Cap expensive provider scans while preserving high confidence for
            // "most recent" results. Workspace-scoped lists can use a tighter cap.
            let multiplier = if workspace_scoped { 3 } else { 8 };
            std::cmp::max(limit.saturating_mul(multiplier), 30)
        } else {
            usize::MAX
        }
    }

    /// What a provider's on-disk layout says about the workspace a session
    /// belongs to.
    ///
    /// Three answers, not two, because "this layout encodes no workspace" is a
    /// different fact from "this layout encodes a different workspace". The
    /// `bool` this replaced returned `true` for both, so `true` meant "matches"
    /// for the two providers whose directory names are derived from a
    /// workspace path and "no opinion" for the other fifteen — and the only
    /// thing that could tell those two readings apart was a hardcoded list of
    /// the two slugs, consulted by the caller. Naming the third answer deletes
    /// the list: every provider without a workspace-derived layout reports
    /// [`WorkspaceHint::Unknown`] by falling off the end of the match, which is
    /// exactly what it means.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum WorkspaceHint {
        /// The layout places this session in the filtered workspace.
        Matches,
        /// The layout places this session in some other workspace.
        Differs,
        /// The layout says nothing about which workspace this session is from.
        Unknown,
    }

    fn workspace_path_hint(
        provider_slug: &str,
        path: &Path,
        workspace_filter: Option<&PathBuf>,
    ) -> WorkspaceHint {
        let Some(ws) = workspace_filter else {
            return WorkspaceHint::Unknown;
        };

        match provider_slug {
            "claude-code" => {
                let expected = casr::providers::claude_code::project_dir_key(ws.as_path());
                match path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                {
                    Some(key) if key == expected => WorkspaceHint::Matches,
                    Some(_) => WorkspaceHint::Differs,
                    // No parent directory to read a project key from.
                    None => WorkspaceHint::Unknown,
                }
            }
            "gemini" => {
                // `tmp/<id>/chats/<file>` — the project directory is the
                // grandparent. Which layout named it, and whether that name or
                // the marker beside it answers, is the provider's to know:
                // testing for a 64-hex directory here reported `Unknown` for
                // every session a current Gemini writes, because 0.52.0 names
                // the directory with a registry slug.
                let by_directory = path.parent().and_then(|p| p.parent()).and_then(|dir| {
                    casr::providers::gemini::project_dir_matches(dir, ws.as_path())
                });
                // Neither marked nor hashed — but the file itself is a second
                // witness, and an independent one: Gemini stamps
                // `SHA256(projectRoot)` into every session header it writes
                // (`chatRecordingService.js:328`). Consulted only here, after
                // the directory has declined, because where the two can
                // disagree the directory is right — see
                // `gemini::session_workspace_hint`.
                //
                // This does not answer for a directory, and the `--workspace`
                // fast path below still refuses to classify a tree from the
                // part of it that happened to classify. It narrows which
                // *sessions* end up in the "workspace could not be determined"
                // count, which before this was every session in any project
                // directory whose marker was missing or unreadable.
                let resolved = by_directory.or_else(|| {
                    casr::providers::gemini::session_workspace_hint(path, ws.as_path())
                });
                match resolved {
                    Some(true) => WorkspaceHint::Matches,
                    Some(false) => WorkspaceHint::Differs,
                    None => WorkspaceHint::Unknown,
                }
            }
            _ => WorkspaceHint::Unknown,
        }
    }

    fn workspace_scoped_listed_sessions(
        provider_slug: &str,
        workspace_filter: Option<&PathBuf>,
    ) -> Option<SessionListing> {
        let ws = workspace_filter?;
        match provider_slug {
            "claude-code" => {
                // Reuse the provider's own resolver so this fast path cannot
                // drift from the env-var precedence the provider implements.
                let claude_home = casr::providers::claude_code::ClaudeCode::home_dir()?;
                let expected_dir = claude_home
                    .join("projects")
                    .join(casr::providers::claude_code::project_dir_key(ws.as_path()));

                let mut listing = SessionListing::default();
                for entry in read_dir_reporting(&expected_dir, &mut listing.unreadable) {
                    let path = entry.path();
                    if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl")
                    {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    listing.sessions.push((stem.to_string(), path));
                }
                Some(listing)
            }
            "gemini" => {
                // Reuse the provider's own resolver so this fast path cannot
                // drift from the env-var precedence the provider implements.
                let gemini_home = casr::providers::gemini::Gemini::home_dir()?;
                let tmp_root = gemini_home.join("tmp");

                // One pass over `tmp/`, asking the provider about each project
                // directory, because a workspace can own two of them at once:
                // 0.52.0 migrates by *copying* the pre-0.52.0 `SHA256(ws)`
                // directory into its new slug directory and leaving the
                // original behind. Computing the hash path and stopping there
                // found the frozen copy and hid every session written since —
                // and on a machine with no pre-0.52.0 history it found nothing
                // at all.
                let mut listing = SessionListing::default();
                let mut chats_dirs: Vec<PathBuf> = Vec::new();
                let mut undetermined = false;
                for entry in read_dir_reporting(&tmp_root, &mut listing.unreadable) {
                    let dir = entry.path();
                    let chats = dir.join("chats");
                    if !chats.is_dir() {
                        continue;
                    }
                    match casr::providers::gemini::project_dir_matches(&dir, ws.as_path()) {
                        Some(true) => chats_dirs.push(chats),
                        Some(false) => {}
                        None => undetermined = true,
                    }
                }
                if undetermined {
                    // Hand the whole question to the provider, which will
                    // report its own failures; this scan's are its to find
                    // again rather than to double-report here.
                    //
                    // Any undetermined directory forces this, not just one
                    // that leaves no matches: answering from the directories
                    // that *did* classify would drop the rest without the
                    // caller ever counting them, so `--workspace` would go
                    // back to hiding sessions silently — the failure this
                    // whole path exists downstream of.
                    return None;
                }

                // Same acceptance rule as the provider, for the same reason:
                // gating on `.json` here hid every JSONL session from `list`
                // independently of the reader, so fixing one without the other
                // fixes nothing the user can see. `is_session_file_name` is the
                // provider's, so the two cannot drift apart again.
                //
                // Keyed by the *file* id rather than the session id the body
                // declares, because this fast path exists to avoid opening the
                // files at all. A migrated session is `session-<ts>-<id8>.json`
                // and `session-<ts>-<id8>.jsonl`, which share that key, so the
                // pair still collapses to the `.jsonl`. The same key also
                // collapses the two copies a migration leaves of one session.
                let mut seen: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for chats_dir in &chats_dirs {
                    for entry in read_dir_reporting(chats_dir, &mut listing.unreadable) {
                        let path = entry.path();
                        if !path.is_file() {
                            continue;
                        }
                        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        if !casr::providers::gemini::is_session_file_name(name) {
                            continue;
                        }
                        let session_id = name
                            .strip_prefix("session-")
                            .map(|rest| {
                                rest.strip_suffix(".jsonl")
                                    .or_else(|| rest.strip_suffix(".json"))
                                    .unwrap_or(rest)
                            })
                            .unwrap_or(name)
                            .to_string();
                        match seen.get(&session_id) {
                            Some(&index) => {
                                let live_is_jsonl = listing.sessions[index]
                                    .1
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    == Some("jsonl");
                                if !live_is_jsonl
                                    && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                                {
                                    listing.sessions[index].1 = path;
                                }
                            }
                            None => {
                                seen.insert(session_id.clone(), listing.sessions.len());
                                listing.sessions.push((session_id, path));
                            }
                        }
                    }
                }
                Some(listing)
            }
            _ => None,
        }
    }

    let workspace_filter_explicit = workspace_filter.is_some();
    let workspace_filter = workspace_filter
        .map(expand_tilde_path)
        .or_else(|| std::env::current_dir().ok());
    let workspace_scope = workspace_filter
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "all workspaces".to_string());
    let workspace_scope_label = if workspace_filter_explicit {
        "workspace project (--workspace)"
    } else {
        "current working-directory project"
    };
    tracing::debug!(
        provider_filter = ?provider_filter_slug,
        workspace = %workspace_scope,
        scope = %workspace_scope_label,
        sort,
        limit,
        "listing sessions"
    );

    let mut sessions: Vec<SessionSummary> = Vec::new();
    let mut skipped: Vec<SkippedSession> = Vec::new();

    const LIST_PARSE_PARALLEL_THRESHOLD: usize = 256;

    /// What one candidate file became.
    ///
    /// Three outcomes, not two, for the reason [`WorkspaceHint`] has three
    /// answers: "the layout places this file in another workspace" is a
    /// measurement that answers the user's question, and "the reader could not
    /// open it" is a failure to measure anything. The `filter_map` this
    /// replaced returned `None` for both — `read_session(&path).ok()?` — so a
    /// file that could not be parsed left the listing by the same door as a
    /// file that was correctly excluded, and the listing had no way to tell the
    /// user which had happened. One unreadable file must not end the listing,
    /// so a failure is carried out as a value rather than raised.
    enum Candidate {
        /// Read, and belongs in the listing.
        Row(SessionSummary),
        /// Excluded on purpose: this provider's layout places it elsewhere.
        Elsewhere,
        /// Found, claimed as a session, and unreadable.
        Unreadable(SkippedSession),
    }

    /// Read one candidate, keeping the reason when it cannot be read.
    fn classify_candidate(
        provider: &dyn Provider,
        provider_slug: &str,
        path: PathBuf,
        workspace_filter: Option<&PathBuf>,
    ) -> Candidate {
        // Only a positive mismatch skips the parse. `Unknown` is not evidence,
        // so it must not act like one.
        if workspace_path_hint(provider_slug, &path, workspace_filter) == WorkspaceHint::Differs {
            return Candidate::Elsewhere;
        }
        match provider.read_session(&path) {
            Ok(session) => Candidate::Row(build_summary(provider_slug, path, session)),
            Err(error) => Candidate::Unreadable(SkippedSession {
                provider: provider_slug.to_string(),
                path: path.display().to_string(),
                error: format!("{error}"),
            }),
        }
    }

    for provider in &installed {
        tracing::debug!(provider = provider.slug(), "scanning provider for sessions");
        if let Some(filter_slug) = provider_filter_slug.as_deref()
            && provider.slug() != filter_slug
            && provider.cli_alias() != filter_slug
        {
            continue;
        }

        // Prefer list_sessions() for providers that store multiple sessions
        // in a single file/DB (avoids undercounting).
        let scoped_listed =
            workspace_scoped_listed_sessions(provider.slug(), workspace_filter.as_ref());
        if let Some(listing) = scoped_listed.or_else(|| provider.list_sessions()) {
            // The places this provider could not look, before anything is
            // truncated. They are not sessions and must not be subject to the
            // probe limit: a listing capped at 20 rows that silently drops the
            // one directory it was refused is the defect wearing a cap.
            for source in listing.unreadable {
                skipped.push(SkippedSession {
                    provider: provider.slug().to_string(),
                    path: source.path.display().to_string(),
                    error: source.error,
                });
            }

            let mut listed = listing.sessions;
            let probe_limit = probe_limit_for_sort(limit, sort, workspace_filter.is_some());
            if listed.len() > probe_limit {
                listed.sort_by_key(|(_, path)| std::cmp::Reverse(file_mtime_millis(path)));
                listed.truncate(probe_limit);
            }

            let provider_slug = provider.slug().to_string();
            let classify = |path: PathBuf| {
                classify_candidate(*provider, &provider_slug, path, workspace_filter.as_ref())
            };
            let parsed: Vec<Candidate> = if listed.len() < LIST_PARSE_PARALLEL_THRESHOLD {
                listed
                    .into_iter()
                    .map(|(_session_id, path)| classify(path))
                    .collect()
            } else {
                listed
                    .into_par_iter()
                    .map(|(_session_id, path)| classify(path))
                    .collect()
            };
            for candidate in parsed {
                match candidate {
                    Candidate::Row(summary) => sessions.push(summary),
                    Candidate::Elsewhere => {}
                    Candidate::Unreadable(skip) => skipped.push(skip),
                }
            }
            continue;
        }

        let mut candidate_paths: Vec<PathBuf> = Vec::new();

        for root in provider.session_roots() {
            let mut unreadable = Vec::new();
            for entry in walkdir::WalkDir::new(&root).max_depth(4) {
                // A subtree the walker was refused used to leave by the same
                // door as a file that is not a session — `filter_map(Result::ok)`
                // — so a provider whose store had become unreadable reported
                // "no sessions" instead of saying so.
                let Some(entry) = walk_entry_reporting(entry, &mut unreadable) else {
                    continue;
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                // The provider's own rule for what it writes, not "every file
                // with a plausible extension". The latter is how ClawdBot's
                // `sessions.json`, Factory's `<sessionId>.settings.json` and
                // Vibe's `meta.json` were rendered as sessions with zero
                // messages.
                if !provider.is_session_path(path) {
                    continue;
                }

                if workspace_path_hint(provider.slug(), path, workspace_filter.as_ref())
                    == WorkspaceHint::Differs
                {
                    continue;
                }

                candidate_paths.push(path.to_path_buf());
            }
            for source in unreadable {
                skipped.push(SkippedSession {
                    provider: provider.slug().to_string(),
                    path: source.path.display().to_string(),
                    error: source.error,
                });
            }
        }

        let probe_limit = probe_limit_for_sort(limit, sort, workspace_filter.is_some());
        if candidate_paths.len() > probe_limit {
            candidate_paths.sort_by_key(|path| std::cmp::Reverse(file_mtime_millis(path)));
            candidate_paths.truncate(probe_limit);
        }

        let provider_slug = provider.slug().to_string();
        let classify = |path: PathBuf| {
            classify_candidate(*provider, &provider_slug, path, workspace_filter.as_ref())
        };
        let parsed: Vec<Candidate> = if candidate_paths.len() < LIST_PARSE_PARALLEL_THRESHOLD {
            candidate_paths.into_iter().map(classify).collect()
        } else {
            candidate_paths.into_par_iter().map(classify).collect()
        };
        for candidate in parsed {
            match candidate {
                Candidate::Row(summary) => sessions.push(summary),
                Candidate::Elsewhere => {}
                Candidate::Unreadable(skip) => skipped.push(skip),
            }
        }
    }

    // Sessions no source could place in *any* workspace, counted per provider.
    //
    // Two independent sources can place a session: the workspace its reader
    // recorded, and the provider's on-disk layout. Either saying "this one is
    // in the filtered workspace" keeps it; either saying "this one is
    // elsewhere", with nothing contradicting it, drops it. When both come back
    // `Unknown` the session is *unclassified*, which is not the same as
    // disqualified, and the previous filter collapsed the two: it kept a
    // session only if it had a recorded workspace or its provider appeared in
    // a two-name allowlist. Since the filter falls back to the working
    // directory when `--workspace` is absent, that made every session with no
    // recorded workspace unlistable everywhere — permanently for the four
    // providers that never record one, and per-session for the thirteen whose
    // source file may not carry a `cwd`.
    //
    // What an unclassified session should do depends on whether the user
    // actually asked a workspace question. With an explicit `--workspace X`
    // they did, and "we cannot tell whether this is in X" is not an answer to
    // it, so the session stays out — but the exclusion is now reported instead
    // of silent. Without the flag they asked for a list; the working directory
    // is a default scope, not a claim, and answering it by deleting everything
    // unmeasurable is the silent-default failure this codebase reports `null`
    // to avoid everywhere else.
    let mut unclassified: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if let Some(filter) = workspace_filter.as_ref() {
        sessions.retain(|s| {
            let recorded = s.workspace.as_ref().map(|w| w.starts_with(filter));
            let hint = workspace_path_hint(&s.provider, &s.path, Some(filter));
            match (recorded, hint) {
                (_, WorkspaceHint::Matches) | (Some(true), _) => true,
                (Some(false), _) | (_, WorkspaceHint::Differs) => false,
                (None, WorkspaceHint::Unknown) => {
                    *unclassified.entry(s.provider.clone()).or_default() += 1;
                    !workspace_filter_explicit
                }
            }
        });
    }

    // Say so. A filter that removes what it could not classify, without
    // mentioning it, is indistinguishable from a provider having no sessions.
    if workspace_filter_explicit && !unclassified.is_empty() {
        let total: usize = unclassified.values().sum();
        let providers: Vec<&str> = unclassified.keys().map(String::as_str).collect();
        eprintln!(
            "{} {total} session(s) hidden by --workspace: their workspace could not be \
             determined ({}). Run without --workspace to include them.",
            "⚠".yellow(),
            providers.join(", ")
        );
    }

    // Say so here too, and for the same reason: a listing that is short because
    // some of it would not parse looks exactly like a listing that is short.
    //
    // # Why stderr, and why only without `--json`
    //
    // stdout is this command's data channel — `--json` callers parse it, and
    // `list` already sends its other two diagnostics (a store that would not
    // open, and sessions hidden by `--workspace`) to stderr. Under `--json` the
    // envelope carries every one of these facts in full, so repeating them on
    // stderr would only duplicate the document, which is the argument
    // [`launch_line`] already makes for the launch line. Under plain `list`
    // there is no envelope, so stderr is the only place left.
    //
    // # Why a count, then examples, then a number
    //
    // The count is the fact that changes what the user believes about the list
    // they just read. The reasons are what make it actionable. Printing every
    // reason is not: a provider whose reader has broken will produce one line
    // per session and bury the listing it was supposed to annotate, so three
    // are shown and the rest are counted, with `--json` named as the place that
    // holds all of them. One broken file is under the cap and names itself,
    // which is the case a user most needs the path for.
    const SKIPPED_EXAMPLES: usize = 3;
    if !skipped.is_empty() && !json_mode {
        let mut by_provider: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for skip in &skipped {
            *by_provider.entry(skip.provider.as_str()).or_default() += 1;
        }
        let counts: Vec<String> = by_provider
            .iter()
            .map(|(provider, count)| format!("{provider}: {count}"))
            .collect();
        eprintln!(
            "{} {} path(s) could not be read; any sessions in them are missing from this \
             listing ({}).",
            "⚠".yellow(),
            skipped.len(),
            counts.join(", ")
        );
        for skip in skipped.iter().take(SKIPPED_EXAMPLES) {
            eprintln!("  {} — {}", skip.path, skip.error);
        }
        if skipped.len() > SKIPPED_EXAMPLES {
            eprintln!(
                "  … and {} more; `casr list --json` reports every one.",
                skipped.len() - SKIPPED_EXAMPLES
            );
        }
    }

    let mut sessions_by_provider: std::collections::BTreeMap<String, Vec<SessionSummary>> =
        std::collections::BTreeMap::new();
    for session in sessions {
        sessions_by_provider
            .entry(session.provider.clone())
            .or_default()
            .push(session);
    }

    for provider_sessions in sessions_by_provider.values_mut() {
        match sort {
            "date" => provider_sessions.sort_by_key(|s| std::cmp::Reverse(s.recency_value())),
            "messages" => provider_sessions.sort_by(|a, b| {
                b.messages
                    .cmp(&a.messages)
                    .then_with(|| b.recency_value().cmp(&a.recency_value()))
            }),
            "provider" => provider_sessions.sort_by_key(|s| std::cmp::Reverse(s.recency_value())),
            other => {
                return Err(anyhow::anyhow!(
                    "Unknown sort field '{other}'. Expected one of: date, messages, provider."
                ));
            }
        }
        provider_sessions.truncate(limit);
    }

    let non_empty_group_count = sessions_by_provider
        .values()
        .filter(|sessions| !sessions.is_empty())
        .count();
    let total_sessions_kept: usize = sessions_by_provider.values().map(Vec::len).sum();
    tracing::debug!(
        providers = non_empty_group_count,
        sessions = total_sessions_kept,
        sort,
        limit,
        "list sessions complete"
    );

    if json_mode {
        let mut items: Vec<ListItem> = Vec::new();
        for sessions in sessions_by_provider.values() {
            for session in sessions {
                items.push(session.to_list_item(enrich_fs));
            }
        }
        let envelope = ListEnvelope::new(items, skipped);
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        if non_empty_group_count == 0 {
            println!(
                "No sessions found for {} {}. Run {} to check provider status.",
                workspace_scope_label.cyan(),
                workspace_scope.cyan(),
                "casr providers".cyan(),
            );
            return Ok(());
        }

        let console = Console::new();
        console.print(&format!(
            "[bold cyan]Project-scoped sessions[/] for [bold]{workspace_scope}[/]"
        ));
        console.print(&format!("[dim]Scope:[/] [bold]{workspace_scope_label}[/]"));
        // "Project-scoped" is a claim, and it is not true of every row here:
        // some readers cannot tell which workspace a session came from, and
        // those are shown rather than dropped. Say which ones, so the heading
        // is not read as a measurement nobody made.
        let unplaced_shown: usize = sessions_by_provider
            .values()
            .flatten()
            .filter(|s| s.workspace.is_none())
            .count();
        if unplaced_shown > 0 {
            console.print(&format!(
                "[dim]Includes[/] [bold]{unplaced_shown}[/] [dim]session(s) that record no \
                 workspace — shown because nothing places them outside this project.[/]"
            ));
        }
        console.print(&format!(
            "[dim]Showing up to[/] [bold]{limit}[/] [dim]most recent sessions per provider[/]"
        ));

        let now_millis = Utc::now().timestamp_millis();

        for (provider_slug, provider_sessions) in &sessions_by_provider {
            if provider_sessions.is_empty() {
                continue;
            }

            let provider = provider_display(provider_slug);
            console.print(&format!(
                "[bold]{}[/]: {} session(s)",
                provider,
                provider_sessions.len()
            ));

            let mut table = Table::new()
                .title(format!(
                    "Top {} Most Recently Active {} Sessions in This Project",
                    provider_sessions.len(),
                    provider
                ))
                .header_style(Style::parse("bold black on bright_white").unwrap_or_default())
                .border_style(Style::parse("cyan").unwrap_or_default())
                .with_column(Column::new("#").justify(JustifyMethod::Right).width(3))
                .with_column(Column::new("Session ID").min_width(36))
                .with_column(Column::new("Name").justify(JustifyMethod::Left).width(24))
                .with_column(Column::new("Msgs").justify(JustifyMethod::Right).width(6))
                .with_column(
                    Column::new("Size KB")
                        .justify(JustifyMethod::Right)
                        .width(8),
                )
                .with_column(
                    Column::new("Unique Users")
                        .justify(JustifyMethod::Right)
                        .width(12),
                )
                .with_column(
                    Column::new("Agent Avg Chars")
                        .justify(JustifyMethod::Right)
                        .width(15),
                )
                .with_column(
                    Column::new("Tool Uses")
                        .justify(JustifyMethod::Right)
                        .width(10),
                )
                .with_column(
                    Column::new("Started")
                        .justify(JustifyMethod::Left)
                        .width(16),
                )
                .with_column(
                    Column::new("Last Active")
                        .justify(JustifyMethod::Left)
                        .min_width(22),
                );

            for (idx, s) in provider_sessions.iter().enumerate() {
                let rank = (idx + 1).to_string();
                let session_id = s.session_id.as_str();
                let native_name = s
                    .native_name
                    .as_deref()
                    .map(|name| truncate_display_name(name, 22))
                    .unwrap_or_default();
                let messages = s.messages.to_string();
                let messages_cell_style = message_count_style(s.messages);
                let size_kb = s.file_size_display();
                let unique_users = format_with_commas(s.unique_user_messages as u64);
                let avg_agent = s.avg_agent_chars_display();
                // `?` rather than `0`: no provider scanner could count these,
                // and a column of zeroes reads as "this agent uses no tools".
                let tool_uses = s
                    .tool_uses
                    .map_or_else(|| "?".to_string(), |n| format_with_commas(n as u64));
                let started = s.started_at_display();
                let last_active = s.last_active_display(now_millis);
                let last_active_cell_style = last_active_style(s.last_active_at, now_millis);
                table.add_row(Row::new(vec![
                    Cell::new(rank.as_str()),
                    Cell::new(session_id),
                    Cell::new(native_name.as_str()),
                    Cell::new(messages.as_str()).style(messages_cell_style),
                    Cell::new(size_kb.as_str()),
                    Cell::new(unique_users.as_str()),
                    Cell::new(avg_agent.as_str()),
                    Cell::new(tool_uses.as_str()),
                    Cell::new(started.as_str()),
                    Cell::new(last_active.as_str()).style(last_active_cell_style),
                ]));
            }

            console.print_renderable(&table);
        }
        console.print("[dim]Tip:[/] run [bold]casr info <session-id>[/] for full metadata.");
    }

    Ok(())
}

fn cmd_info(
    argument: &str,
    json_mode: bool,
    enrich_fs: bool,
    source: Option<String>,
    from: Option<String>,
    peek: bool,
    peek_lines: Option<usize>,
) -> anyhow::Result<()> {
    let registry = ProviderRegistry::default_registry();

    // A path and an ID are told apart by the filesystem, as the help states: an
    // argument that names an existing file is a path, anything else is an ID.
    // The rule cannot be syntactic, because a Codex session ID *is*
    // `2026/07/27/rollout-…` and a rule about separators would eat it.
    //
    // Routing a path through `SourceHint::Path` also fixes what an ID lookup
    // did with one. `Codex::owns_session` joins the argument onto its sessions
    // directory, and joining an absolute path discards the left side — so a
    // Claude transcript handed to `info` as a path resolved as *Codex*, was
    // parsed by the Codex reader, and reported `provider: "codex"` with zero
    // messages for an 18-message session.
    let as_path = Path::new(argument);
    let path_argument = (as_path.is_file() || as_path.parent().is_some_and(Path::is_file))
        .then(|| as_path.to_path_buf());

    // `--from` forces the reader. Resolved up front so an unknown slug fails
    // before anything is parsed, and so the error names the known aliases.
    let forced = from
        .as_deref()
        .map(|alias| {
            registry.find_by_alias(alias).ok_or_else(|| {
                casr::error::CasrError::UnknownProviderAlias {
                    alias: alias.to_string(),
                    known_aliases: registry.known_aliases(),
                }
            })
        })
        .transpose()?;

    let (provider, path): (&dyn Provider, PathBuf) = match (forced, path_argument) {
        // A path plus a forced reader needs no resolution at all.
        (Some(provider), Some(path)) => (provider, path),
        // An ID plus a forced reader: that provider is the one asked to find it.
        (Some(provider), None) => {
            let hint = casr::discovery::SourceHint::Alias(provider.slug().to_string());
            (
                provider,
                registry.resolve_session(argument, Some(&hint))?.path,
            )
        }
        // No forced reader: a path argument outranks `--source`, being the more
        // specific of the two, and otherwise `--source` behaves as it always has.
        (None, path_argument) => {
            let hint = path_argument
                .map(casr::discovery::SourceHint::Path)
                .or_else(|| source.as_deref().map(casr::discovery::SourceHint::parse));
            let resolved = registry.resolve_session(argument, hint.as_ref())?;
            (resolved.provider, resolved.path)
        }
    };

    let session = provider.read_session(&path)?;

    // The two tracks can account for different things, and the difference is
    // reported rather than papered over: a structured reader counts real `Body`
    // variants, a flat one counts what a `CanonicalSession` holds and says
    // `null` for the rest. Which track a provider is on is a property of the
    // provider, so asking costs nothing; the second parse is only paid where it
    // buys a real answer.
    //
    // Both questions are answered from the one IR: `summary` is the whole file
    // and `live_summary` is what the replay fold leaves standing. On the flat
    // track they are the same object, because that track has no fold.
    let (summary, live_summary) = match provider
        .supports_structured_read()
        .then(|| provider.read_session_ir(&path))
        .transpose()?
        .flatten()
    {
        Some(ir) => (
            responses::EventSummary::of_ir(&ir),
            responses::EventSummary::of_live(&ir),
        ),
        None => {
            let flat = responses::EventSummary::of_flat(&session);
            (flat.clone(), flat)
        }
    };

    let native_name = casr::model::native_name_from_metadata(&session.metadata);
    // The tail shows when `--peek` is passed OR `--peek-lines N` is given on its
    // own (the latter implies `--peek`, as the help states); default to 5 turns.
    const DEFAULT_PEEK_LINES: usize = 5;
    let show_tail = peek || peek_lines.is_some();
    // Snippet width: wider for JSON (machine-consumed), tighter for the terminal.
    let transcript_tail = show_tail.then(|| {
        let n = peek_lines.unwrap_or(DEFAULT_PEEK_LINES);
        casr::model::transcript_tail(&session.messages, n, PEEK_SNIPPET_MAX_CHARS)
    });

    if json_mode {
        let (workspace_name, workspace_name_source) =
            responses::workspace_name_from_path(session.workspace.as_ref());
        let repo_name = if enrich_fs {
            session
                .workspace
                .as_ref()
                .and_then(|ws| casr::discovery::repo_name_from_path(ws))
        } else {
            None
        };
        let response = InfoResponse {
            schema_version: responses::SCHEMA_VERSION,
            session_id: session.session_id.clone(),
            provider: session.provider_slug.clone(),
            detected_format: provider.slug().to_string(),
            title: session.title.clone(),
            native_name: native_name.clone(),
            workspace: session.workspace.as_ref().map(|w| w.display().to_string()),
            messages: session.messages.len(),
            summary,
            live_summary,
            started_at: session.started_at,
            ended_at: session.ended_at,
            model_name: session.model_name.clone(),
            source_path: session.source_path.display().to_string(),
            metadata: session.metadata.clone(),
            workspace_name,
            workspace_name_source,
            repo_name,
            transcript_tail,
        };
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("{}\n", "Session Info".bold());
        println!("  {} {}", "ID:".dimmed(), session.session_id.cyan());
        println!("  {} {}", "Provider:".dimmed(), session.provider_slug);
        // Only when it disagrees. Repeating the provider slug on the line below
        // itself is noise; a reader that read the session as something other
        // than what the session says it is, is the whole message.
        if provider.slug() != session.provider_slug {
            println!("  {} {}", "Read as:".dimmed(), provider.slug().yellow());
        }
        if let Some(ref name) = native_name {
            println!("  {} {name}", "Name:".dimmed());
        }
        if let Some(ref title) = session.title {
            println!("  {} {title}", "Title:".dimmed());
        }
        if let Some(ref ws) = session.workspace {
            println!("  {} {}", "Workspace:".dimmed(), ws.display());
        }
        println!("  {} {}", "Messages:".dimmed(), session.messages.len());
        if let Some(ref model) = session.model_name {
            println!("  {} {model}", "Model:".dimmed());
        }
        println!("  {} {}", "Path:".dimmed(), session.source_path.display());

        // Show role breakdown.
        let user_count = session
            .messages
            .iter()
            .filter(|m| m.role == casr::model::MessageRole::User)
            .count();
        let asst_count = session
            .messages
            .iter()
            .filter(|m| m.role == casr::model::MessageRole::Assistant)
            .count();
        println!(
            "  {} {user_count} user, {asst_count} assistant",
            "Roles:".dimmed()
        );

        // `?` where the reader cannot tell, never 0. Zero counts are left out
        // entirely — they say nothing a reader can act on, and the point of the
        // line is what the session has and what cannot be known about it.
        //
        // The second line is shown only when the fold actually removed
        // something. Where they agree, repeating the same numbers under a
        // different label reads as two facts when there is one.
        println!("  {} {}", "Events:".dimmed(), summary.describe());
        if live_summary != summary {
            println!("  {} {}", "Live:".dimmed(), live_summary.describe());
        }

        if let Some(ref tail) = transcript_tail {
            println!(
                "\n{}",
                format!("Transcript Tail (last {} turns)", tail.len()).bold()
            );
            if tail.is_empty() {
                println!("  {}", "(no messages)".dimmed());
            }
            for turn in tail {
                println!("  {} {}", format!("[{}]", turn.role).cyan(), turn.snippet);
            }
        }
    }

    Ok(())
}

fn cmd_providers(json_mode: bool) -> anyhow::Result<()> {
    let registry = ProviderRegistry::default_registry();
    let results = registry.detect_all();

    if json_mode {
        let providers: Vec<ProviderInfo> = results
            .iter()
            .map(|(p, det)| ProviderInfo {
                name: p.name().to_string(),
                slug: p.slug().to_string(),
                alias: p.cli_alias().to_string(),
                installed: det.installed,
                version: det.version.clone(),
                evidence: det.evidence.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&providers)?);
    } else {
        println!("{}\n", "Detected Providers".bold());
        for (provider, detection) in &results {
            let status = if detection.installed {
                "✓".green().bold().to_string()
            } else {
                "✗".red().bold().to_string()
            };
            println!(
                "  {status} {} ({}) — alias: {}",
                provider.name(),
                provider.slug(),
                provider.cli_alias().cyan()
            );
            for ev in &detection.evidence {
                println!("    {ev}");
            }
        }
    }

    Ok(())
}

fn cmd_completions(shell: &str) -> anyhow::Result<()> {
    use clap::CommandFactory;
    use clap_complete::{Shell, generate};

    let parsed_shell: Shell = shell
        .parse()
        .map_err(|_| anyhow::anyhow!("Unknown shell '{shell}'. Use: bash, zsh, fish"))?;

    let mut cmd = Cli::command();
    generate(parsed_shell, &mut cmd, "casr", &mut std::io::stdout());

    Ok(())
}
