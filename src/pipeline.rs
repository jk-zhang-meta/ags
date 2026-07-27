//! Conversion pipeline orchestrator.
//!
//! Ties detection, reading, validation, writing, and verification into a
//! single `convert()` call. Generic over the [`Provider`](crate::providers::Provider)
//! trait — concrete providers are wired in via the registry.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use tracing::{debug, info, warn};

use crate::discovery::{ProviderRegistry, ResolvedSession, SourceHint};
use crate::error::CasrError;
use crate::ir::{Body, Fidelity, Loss, LossKind, SessionIr};
use crate::model::{CanonicalMessage, CanonicalSession, MessageRole, reindex_messages};
use crate::providers::{Provider, StructuredWrite, WriteOptions, WrittenSession};
use crate::store::{DerivedWrite, OriginPolicy, SessionKey, SourceCandidate, SourceChoice, Store};

/// Top-level orchestrator for session conversion.
pub struct ConversionPipeline {
    pub registry: ProviderRegistry,
    /// The session store, when one is in use.
    ///
    /// `None` is `--no-store`, and it is the *absence* of a store rather than a
    /// second code path: with no store there is nothing to ask, so
    /// [`ConversionPipeline::convert`] reads the session it was given, writes
    /// where it was told, and records nothing — which is exactly what it did
    /// before the store existed.
    ///
    /// Owned rather than borrowed so that the pipeline holds one store for the
    /// same reason it holds one registry, and so that
    /// [`crate::store::Store::best_source_for`] can be handed *this* registry
    /// instead of building a second one that could disagree with it.
    pub store: Option<Store>,
}

/// Which incarnation of a conversation the store chose to read, and what the
/// alternatives would have cost.
///
/// Present on [`ConversionResult`] rather than only in a log line, because the
/// store may read a session the user did not name and that has to be visible in
/// the result. The rendering split is the store's:
/// [`crate::store::SourceChoice`] decided without being told what the user
/// asked for, and only [`SourceSelection::line`] is told.
#[derive(Debug, Clone)]
pub struct SourceSelection {
    /// The store record this conversation belongs to — our identifier, the one
    /// `resume <record-id>` takes.
    pub record_id: String,
    /// The session the user named, which the store is free not to read.
    pub named: SessionKey,
    /// Every incarnation the store ranked for this target, best first.
    pub choice: SourceChoice,
}

impl SourceSelection {
    /// The incarnation that was actually read.
    ///
    /// [`crate::store::SourceChoice::resolve`] rather than `chosen`, so that a
    /// record the ranking could not settle reads the session the user named —
    /// which is what `--no-store` would have delivered — instead of the head of a
    /// ranking that has no way to prefer one side of a divergence.
    pub fn chosen(&self) -> Option<&SourceCandidate> {
        self.choice.resolve(Some(&self.named))
    }

    /// Whether the store read a session the user did not name.
    ///
    /// The one condition under which this selection has to be reported: when it
    /// agrees with the user there is nothing to tell them.
    pub fn overrode(&self) -> bool {
        self.chosen().is_some_and(|chosen| chosen.key != self.named)
    }

    /// One line naming the source and what not taking the user's suggestion
    /// saved.
    pub fn line(&self) -> String {
        self.choice.explain(Some(&self.named))
    }
}

/// Options passed through the pipeline from CLI flags.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub dry_run: bool,
    pub force: bool,
    pub verbose: bool,
    pub enrich: bool,
    pub source_hint: Option<String>,
    /// How much of the session the caller allowed across, applied only to
    /// cross-provider conversions.
    ///
    /// One value rather than the three flags it is made of, because both tracks
    /// consume it — the flat track reads its fields in [`apply_context_budget`],
    /// the structured track hands it whole to [`Provider::write_session_ir`] —
    /// and two copies of the same three numbers is how the tracks would drift
    /// into disagreeing about what "`--max-tool-output 0`" means.
    ///
    /// [`crate::budget::ContextBudget::UNLIMITED`] is the default and is what a
    /// caller who named no flag gets: nothing is trimmed and no loss is
    /// reported. See [`crate::budget::ContextBudget::requested`].
    pub budget: crate::budget::ContextBudget,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            dry_run: false,
            force: false,
            verbose: false,
            enrich: false,
            source_hint: None,
            // Trimming is something the caller asks for. A conversion nobody
            // constrained carries the whole session.
            budget: crate::budget::ContextBudget::UNLIMITED,
        }
    }
}

/// Outcome of a successful (or dry-run) conversion.
#[derive(Debug)]
pub struct ConversionResult {
    pub source_provider: String,
    pub target_provider: String,
    pub canonical_session: CanonicalSession,
    pub written: Option<WrittenSession>,
    pub warnings: Vec<String>,
    /// How much of the session survived the crossing.
    ///
    /// On the structured track this is whatever the writer reported in
    /// [`StructuredWrite::fidelity`] — it is the only party that knows what it
    /// had to leave behind, so nothing here second-guesses it. On the flat
    /// track the pipeline grades the projection itself; see [`flat_fidelity`].
    pub fidelity: Fidelity,
    /// What [`ConversionResult::fidelity`] is made of.
    ///
    /// A `Fidelity` names the *category* of loss and deliberately carries no
    /// payload; "1 capsule totalling 87 kB, sealed to openai" is the part a
    /// user can act on, and it has to travel somewhere. Structured rather than
    /// pre-rendered prose so that a caller can filter on [`LossKind`] — the
    /// launch refusal cares only about [`LossKind::SealedContext`] — instead of
    /// matching on sentences. Empty means the grade is its track's baseline.
    pub losses: Vec<Loss>,
    /// The grade an independent read-back of the written file supports.
    ///
    /// `None` when no comparison could run: every flat conversion, every dry
    /// run, and the structured writes whose target has no structured reader or
    /// no vendor this build knows. It is *not* `None` for agreement — a
    /// verifier that ran and agreed is a different fact from one that never ran,
    /// and only the first justifies confidence in
    /// [`ConversionResult::fidelity`].
    ///
    /// Kept beside the writer's claim rather than replacing it. See
    /// [`verify_structured_write`] for why both are reported, and
    /// [`ConversionResult::effective_fidelity`] for which one a decision uses.
    pub verified_fidelity: Option<Fidelity>,
    /// Which incarnation the store chose to read, when a store was in use.
    ///
    /// `None` under `--no-store`, and also when the store was asked and had
    /// nothing to say — a conversation it has never seen and could not ingest.
    /// Either way the conversion read the session that was named.
    pub source: Option<SourceSelection>,
}

impl ConversionResult {
    /// The worst grade any party to this conversion could establish.
    ///
    /// What a refusal keys on, and the reason it exists as its own method: the
    /// number *reported* is the writer's, because hiding a disagreement by
    /// substitution is no better than ignoring it, but the number *acted on*
    /// cannot be one an under-reporting writer chose. When the two agree — the
    /// overwhelmingly common case — this is `fidelity` and nothing changes.
    pub fn effective_fidelity(&self) -> Fidelity {
        match self.verified_fidelity {
            Some(verified) => self.fidelity.worse_of(verified),
            None => self.fidelity,
        }
    }

    /// One sentence when the writer and the read-back disagree, `None` when
    /// they agree or nothing checked.
    ///
    /// Rendered here rather than at each call site so the CLI, the JSON
    /// envelope and the launch refusal cannot describe the same disagreement
    /// three different ways.
    pub fn fidelity_disagreement(&self) -> Option<String> {
        let verified = self.verified_fidelity?;
        (verified > self.fidelity).then(|| {
            format!(
                "The writer graded this conversion {:?}, but reading the written file back \
                 independently only supports {verified:?}; the stricter grade is the one being \
                 acted on.",
                self.fidelity
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Session validation
// ---------------------------------------------------------------------------

/// Result of validating a canonical session.
#[derive(Debug, Clone, Default)]
pub struct ValidationResult {
    /// Fatal issues — pipeline must stop.
    pub errors: Vec<String>,
    /// Non-fatal issues — surfaced in UX/JSON but conversion continues.
    pub warnings: Vec<String>,
    /// Informational notes — shown in verbose/trace mode.
    pub info: Vec<String>,
}

impl ValidationResult {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Validate a canonical session for completeness and quality.
///
/// Returns errors (fatal), warnings (non-fatal), and info notes.
pub fn validate_session(session: &CanonicalSession) -> ValidationResult {
    let mut result = ValidationResult::default();

    // ERRORS — pipeline stops.
    if session.messages.is_empty() {
        result.errors.push("Session has no messages.".to_string());
        return result; // No point checking further.
    }

    let has_user = session.messages.iter().any(|m| m.role == MessageRole::User);
    let has_assistant = session
        .messages
        .iter()
        .any(|m| m.role == MessageRole::Assistant);

    if !has_user || !has_assistant {
        result.errors.push(
            "Session must have at least one user message and one assistant message.".to_string(),
        );
    }

    // WARNINGS — conversion continues.
    if session.workspace.is_none() {
        result.warnings.push(
            "Session has no workspace. Target agent may not know which project to work in."
                .to_string(),
        );
    }

    let has_timestamps = session.messages.iter().any(|m| m.timestamp.is_some());
    if !has_timestamps {
        result
            .warnings
            .push("Session has no timestamps. Message ordering may be unreliable.".to_string());
    }

    if session.messages.len() < 3 {
        result.warnings.push(
            "Very short session (<3 messages). May not provide enough context for resumption."
                .to_string(),
        );
    }

    // INFO — verbose/trace only.
    let has_tool_calls = session.messages.iter().any(|m| !m.tool_calls.is_empty());
    if has_tool_calls {
        result.info.push(
            "Session contains tool calls. Tool semantics may not translate perfectly between providers."
                .to_string(),
        );
    }

    let mut known_tool_call_ids: HashSet<&str> = HashSet::new();
    for msg in &session.messages {
        for call in &msg.tool_calls {
            if let Some(call_id) = call.id.as_deref() {
                known_tool_call_ids.insert(call_id);
            }
        }
    }

    for msg in &session.messages {
        for tool_result in &msg.tool_results {
            if let Some(call_id) = tool_result.call_id.as_deref()
                && !known_tool_call_ids.contains(call_id)
            {
                result.info.push(format!(
                    "Tool result at message index {} references unknown tool call id '{call_id}'.",
                    msg.idx
                ));
                break;
            }
        }
    }

    result
}

fn prepend_enrichment_messages(
    session: &mut CanonicalSession,
    source_provider: &str,
    target_provider: &str,
    source_session_id: &str,
) -> usize {
    let first_timestamp = session.messages.iter().filter_map(|m| m.timestamp).min();
    let notice_timestamp = first_timestamp.map(|ts| ts.saturating_sub(2));
    let summary_timestamp = notice_timestamp.map(|ts| ts.saturating_add(1));

    let mut notice_lines = vec![
        "[casr synthetic context]".to_string(),
        format!(
            "This session was originally created in {source_provider} and converted to {target_provider} format by casr."
        ),
        format!("Original session ID: {source_session_id}."),
        "Some provider-specific context may have been lost in conversion.".to_string(),
        format!("Original message count: {}.", session.messages.len()),
    ];
    if let Some(workspace) = &session.workspace {
        notice_lines.push(format!("Workspace: {}", workspace.display()));
    }

    let (summary_count, summary_lines) = build_recent_summary(session, 4, 180);
    let summary_body = format!(
        "[casr synthetic context]\nRecent conversation snapshot (last {summary_count} message(s)):\n{summary_lines}"
    );

    let notice = CanonicalMessage {
        idx: 0,
        role: MessageRole::System,
        content: notice_lines.join("\n"),
        timestamp: notice_timestamp,
        author: Some("casr-enrichment".to_string()),
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        extra: serde_json::json!({
            "casr_enrichment": true,
            "synthetic": true,
            "enrichment_type": "conversion_notice",
            "source_provider": source_provider,
            "target_provider": target_provider,
            "source_session_id": source_session_id,
        }),
    };

    let summary = CanonicalMessage {
        idx: 1,
        role: MessageRole::System,
        content: summary_body,
        timestamp: summary_timestamp,
        author: Some("casr-enrichment".to_string()),
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        extra: serde_json::json!({
            "casr_enrichment": true,
            "synthetic": true,
            "enrichment_type": "recent_summary",
            "source_provider": source_provider,
            "target_provider": target_provider,
            "source_session_id": source_session_id,
            "summary_message_count": summary_count,
        }),
    };

    let inserted = 2;
    session.messages.insert(0, summary);
    session.messages.insert(0, notice);
    reindex_messages(&mut session.messages);
    inserted
}

fn build_recent_summary(
    session: &CanonicalSession,
    max_messages: usize,
    max_chars_per_message: usize,
) -> (usize, String) {
    let start = session.messages.len().saturating_sub(max_messages);
    let mut lines: Vec<String> = Vec::new();

    for msg in &session.messages[start..] {
        let role = message_role_label(&msg.role);
        let compact_content = compact_summary_text(&msg.content, max_chars_per_message);
        lines.push(format!("- {role}: {compact_content}"));
    }

    if lines.is_empty() {
        lines.push("- (no messages)".to_string());
    }

    (lines.len(), lines.join("\n"))
}

fn compact_summary_text(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "[empty]".to_string();
    }

    let compact_len = compact.chars().count();
    if compact_len <= max_chars {
        return compact;
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut truncated = String::new();
    for ch in compact.chars().take(max_chars - 3) {
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

fn message_role_label(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "assistant".to_string(),
        MessageRole::Tool => "tool".to_string(),
        MessageRole::System => "system".to_string(),
        MessageRole::Other(other) => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Pipeline orchestrator
// ---------------------------------------------------------------------------

impl ConversionPipeline {
    /// Run the full detect → read → validate → write → verify pipeline.
    pub fn convert(
        &self,
        target_alias: &str,
        session_id: &str,
        opts: ConvertOptions,
    ) -> anyhow::Result<ConversionResult> {
        // 1. Resolve target provider.
        let target_provider = self.registry.find_by_alias(target_alias).ok_or_else(|| {
            CasrError::UnknownProviderAlias {
                alias: target_alias.to_string(),
                known_aliases: self.registry.known_aliases(),
            }
        })?;

        info!(
            target = target_provider.name(),
            session_id, "starting conversion"
        );

        let target_detection = target_provider.detect();
        debug!(
            target = target_provider.name(),
            installed = target_detection.installed,
            "target provider detection"
        );
        let mut all_warnings: Vec<String> = Vec::new();
        if !target_detection.installed {
            warn!(
                target = target_provider.name(),
                "target provider CLI not detected; conversion will continue with filesystem-only checks"
            );
            all_warnings.push(format!(
                "Target provider '{}' is not detected as installed. Conversion can still write files, \
but resume may fail until the CLI is installed.",
                target_provider.name()
            ));
        }

        // 2. Resolve source session.
        let source_hint = opts.source_hint.as_deref().map(SourceHint::parse);
        let mut resolved = self
            .registry
            .resolve_session(session_id, source_hint.as_ref())?;

        debug!(
            source = resolved.provider.name(),
            path = %resolved.path.display(),
            "source session resolved"
        );

        // 3. Read source session into canonical IR.
        let mut canonical = resolved.provider.read_session(&resolved.path)?;
        debug!(
            messages = canonical.messages.len(),
            session_id = canonical.session_id,
            "source session read"
        );

        // 3b. Source selection.
        //
        // A conversion is lossy in a direction, so the second hop of
        // `codex → claude → codex` can only carry what the first one left —
        // unless it asks the store, which remembers that both sessions are the
        // same conversation and can hand back the incarnation that still holds
        // the sealed material. Nothing below this line knows the difference:
        // `resolved` and `canonical` are simply the source that was chosen.
        //
        // # Why this sits just after the read and not just before it
        //
        // The store's only external identifier for a session is
        // `(provider, provider_session_id)`, and the provider's own id is not
        // knowable from a path — [`crate::discovery::ResolvedSession`] does not
        // carry one, and `session_id` as typed may be a prefix or a filename
        // suffix that several spellings share. Keying the store on what the user
        // typed would file one conversation under several ids. So the id comes
        // from the read, which costs one wasted flat read in the one case where
        // the store overrides the choice — and that case was going to read a
        // second file anyway.
        let selection = self.select_source(
            target_provider,
            &mut resolved,
            &mut canonical,
            &opts,
            &mut all_warnings,
        );

        // 4. Validate.
        let validation = validate_session(&canonical);
        all_warnings.extend(validation.warnings.clone());

        if validation.has_errors() {
            return Err(CasrError::ValidationError {
                errors: validation.errors,
                warnings: validation.warnings,
                info: validation.info,
            }
            .into());
        }

        for note in &validation.info {
            debug!(note, "validation info");
        }

        // 5. Optional synthetic context enrichment.
        if opts.enrich {
            let source_session_id = canonical.session_id.clone();
            let inserted = prepend_enrichment_messages(
                &mut canonical,
                resolved.provider.slug(),
                target_provider.slug(),
                &source_session_id,
            );
            info!(inserted, "applied casr enrichment");
            all_warnings.push(format!(
                "Added {inserted} synthetic context message(s) via --enrich."
            ));
        }

        // 6. Same-provider short-circuit.
        //
        // Ahead of the dry-run branches rather than behind them, which is where
        // it used to be: a dry run of a same-provider conversion reported
        // whatever the flat projection would have earned, for a conversion that
        // never happens. Predicting the wrong path is the defect the dry run
        // below was rewritten to fix, and this is the first place it occurred.
        if !opts.enrich && resolved.provider.slug() == target_provider.slug() {
            info!("source and target provider are the same — skipping write and verify");
            all_warnings.push(
                "Source and target provider are the same. Skipping conversion write.".to_string(),
            );
            return Ok(ConversionResult {
                source_provider: resolved.provider.slug().to_string(),
                target_provider: target_provider.slug().to_string(),
                canonical_session: canonical.clone(),
                written: Some(WrittenSession {
                    paths: Vec::new(),
                    session_id: canonical.session_id.clone(),
                    resume_command: target_provider.resume_command(&canonical.session_id),
                    backups: Vec::new(),
                    warnings: Vec::new(),
                }),
                warnings: all_warnings,
                // Nothing was converted and nothing was rewritten: the resume
                // command points the agent back at its own bytes.
                fidelity: Fidelity::ByteIdentical,
                losses: Vec::new(),
                verified_fidelity: None,
                // Nothing was written, so there is no new incarnation to
                // record; the source the store chose is already in the record.
                source: selection,
            });
        }

        // 7a1. Track selection.
        //
        // The structured track is taken only when both ends support it: an IR
        // the target cannot consume is no better than no IR, and a structured
        // writer handed a flat projection has nothing extra to write. Either
        // half missing falls through to the flat path below.
        //
        // The target's half is now a capability check rather than a call.
        // `write_session_ir` cannot be asked whether it exists without being
        // handed an IR, so the probe used to cost a full second parse of the
        // source — 281 MiB for the largest rollout in the corpus — to be told
        // `Ok(None)`. [`Provider::supports_structured_write`] answers for free.
        //
        // The *read* below is gated only on the source's own
        // [`Provider::supports_structured_read`], which skips the nineteen
        // providers whose reader would have answered `Ok(None)` anyway. It is
        // deliberately *not* gated on the target's capability, because the IR
        // has a second consumer: [`flat_fidelity`] needs the same IR to see a
        // sealed compaction that the flat projection is about to delete, and
        // grading such a conversion `ConversationOnly` when it is
        // `HistoryIncomplete` is the silent degradation this crate exists to
        // prevent. That gate would buy one parse and sell the grade.
        //
        // Selected here, above the budgeting and tool-normalization steps,
        // because both of those edit `canonical` to suit a flat writer. The
        // structured writer never sees `canonical`, so applying them would be
        // work whose only effect is to misreport the message count; honouring
        // the context budget on this track is the writer's job, over the IR's
        // own `model_visible` view — hence `budget`, which is the same three
        // flags step 7a2 gives the flat projection.
        //
        // `--enrich` is the one thing that takes the structured track away from
        // a pair that both support it, and it has to. Enrichment prepends
        // synthetic messages to `canonical`; the structured writer is handed the
        // untouched source IR and writes it, so the conversion reported "Added
        // N synthetic context messages" over a file that contained none of them,
        // and the read-back verifier — comparing that same untouched IR against
        // what was written — agreed the file was perfect. The flat track is
        // where `canonical` is the thing being written, so it is the track that
        // can keep the promise. A lower grade for an enriched conversion is a
        // true statement; the alternative was a higher one that was false.
        let write_opts = WriteOptions { force: opts.force };
        let budget = opts.budget;
        let source_ir = read_source_ir(resolved.provider, &resolved.path);
        if let Err(detail) = &source_ir {
            all_warnings.push(detail.clone());
        }
        if opts.enrich && target_provider.supports_structured_write() {
            all_warnings.push(
                "--enrich adds messages the structured writers do not carry, so this conversion \
                 ran on the flat track and is graded accordingly."
                    .to_string(),
            );
        }
        if !opts.enrich
            && target_provider.supports_structured_write()
            && let Ok(Some(ir)) = source_ir.as_ref()
        {
            // 7a1a. A dry run stops here, on the track it just selected.
            //
            // Inside the same `if` as the real write, and gated on the same
            // three conditions, because the answer a dry run owes the user is
            // "what will *this* conversion cost" — a prediction computed from a
            // second, parallel decision is a prediction of a different
            // conversion. [`Provider::grade_session_ir`] declines for exactly
            // the reason `write_session_ir` does, so the fall-through below
            // matches too.
            if opts.dry_run {
                if let Some((fidelity, losses)) = target_provider.grade_session_ir(ir, &budget)? {
                    info!(?fidelity, "dry run graded on the structured track");
                    return Ok(ConversionResult {
                        source_provider: resolved.provider.slug().to_string(),
                        target_provider: target_provider.slug().to_string(),
                        canonical_session: canonical,
                        written: None,
                        warnings: all_warnings,
                        fidelity,
                        losses,
                        // Nothing was written, so nothing could be read back.
                        // The one thing a dry run cannot promise, and it says so
                        // by leaving this `None` rather than by guessing.
                        verified_fidelity: None,
                        source: selection,
                    });
                }
            } else if let Some(StructuredWrite {
                written,
                fidelity,
                losses,
            }) = target_provider.write_session_ir(ir, &write_opts, &budget)?
            {
                info!(
                    target_session_id = written.session_id,
                    ?fidelity,
                    "session written on the structured track"
                );
                all_warnings.extend(written.warnings.iter().cloned());

                // 7a1b. Structural read-back verification.
                //
                // The flat verifier cannot be reused here: it compares the target's
                // `read_session` against `canonical`, and `canonical` is not what
                // was written — a structured write that legitimately preserved more
                // than the projection would fail it. So the file is read back
                // through `read_session_ir` and compared IR to IR by
                // [`crate::compare`], whose whole point is that it can tell a
                // predicted vendor-boundary drop from a hole.
                let verified_fidelity;
                match verify_structured_write(target_provider, ir, &budget, &written, fidelity) {
                    Ok((notes, observed)) => {
                        all_warnings.extend(notes);
                        verified_fidelity = observed;
                    }
                    Err(detail) => {
                        warn!(detail, "structured read-back verification failed");
                        let rollback_detail =
                            match rollback_written_session(target_provider.slug(), &written) {
                                Ok(()) => "rollback succeeded".to_string(),
                                Err(rollback_error) => format!("rollback failed: {rollback_error}"),
                            };
                        return Err(CasrError::VerifyFailed {
                            provider: target_provider.slug().to_string(),
                            written_paths: written.paths.clone(),
                            detail: format!("{detail}; {rollback_detail}"),
                        }
                        .into());
                    }
                }

                // 7a1c. Tell the store what we just wrote.
                //
                // After verification, never before it: a record of a conversion is a
                // measurement of an event that happened, and a write that was rolled
                // back did not happen.
                self.record_write(
                    selection.as_ref(),
                    target_provider,
                    &written,
                    fidelity,
                    &losses,
                    &mut all_warnings,
                );

                return Ok(ConversionResult {
                    source_provider: resolved.provider.slug().to_string(),
                    target_provider: target_provider.slug().to_string(),
                    canonical_session: canonical,
                    written: Some(written),
                    warnings: all_warnings,
                    fidelity,
                    losses,
                    verified_fidelity,
                    source: selection,
                });
            }
        }

        // 7a2. Context budget (cross-provider only — same-provider short-circuited above).
        //
        // The Codex reader already collapses the on-disk archive to the live
        // context (honoring compaction). This step keeps that context inside a
        // target-friendly budget: drop the source agent's hidden reasoning if
        // asked, truncate oversized tool observations, then drop the oldest turns if
        // still over the token cap — preserving the original task message and
        // the most recent history, and never severing tool_use/tool_result pairs.
        //
        // The same `budget` the structured branch above hands its writer, so the
        // two tracks cannot answer "was a budget asked for?" differently. When
        // none was, it is `UNLIMITED` and this step removes nothing.
        let (budget_warnings, budget_losses) = apply_context_budget(&mut canonical, &budget);
        all_warnings.extend(budget_warnings);

        // The projection's own grade, then everything the budget removed on top
        // of it. Settled here rather than after the write because nothing below
        // this line can change it — which is what lets a dry run return the same
        // two values without writing anything.
        let (fidelity, losses) = flat_grade(&source_ir, target_provider.slug(), budget_losses);

        // 7a3. A dry run stops here, on the flat track.
        //
        // Below this point is only placement: normalization shapes `canonical`
        // for a writer that will not run, and the write and its read-back cannot
        // happen at all. The grade above is the one the real conversion reports.
        if opts.dry_run {
            info!(?fidelity, "dry run graded on the flat track");
            return Ok(ConversionResult {
                source_provider: resolved.provider.slug().to_string(),
                target_provider: target_provider.slug().to_string(),
                canonical_session: canonical,
                written: None,
                warnings: all_warnings,
                fidelity,
                losses,
                verified_fidelity: None,
                source: selection,
            });
        }

        // 7b. Normalize tool-only messages with empty content.
        //
        // Some source formats (notably Codex with `originator: codex_exec`)
        // produce canonical messages that have empty `content` but non-empty
        // `tool_calls` and/or `tool_results`.  Target writers (e.g. Pi-Agent)
        // either synthesize readable content from tool metadata or emit
        // toolCall blocks that the reader flattens into text on read-back.
        // Unless we mirror that synthesis here the read-back verification
        // will see a content mismatch ("wrote 0 bytes, read back N bytes").
        //
        // Fix: materialise the tool-call/result text into `content` on the
        // canonical message itself so that write ↔ readback is consistent.
        //
        // This step is skipped for structured-tool targets (Claude Code), which
        // round-trip `tool_use` / `tool_result` as native content blocks. Adding
        // a synthesized text block there would corrupt the round-trip and cause
        // the Anthropic API to reject the replayed history alongside the
        // matching `tool_result`.
        let target_preserves_tool_blocks = target_provider.slug() == "claude-code";
        if !target_preserves_tool_blocks {
            for msg in &mut canonical.messages {
                if !msg.content.trim().is_empty() {
                    continue;
                }

                let has_tool_calls = !msg.tool_calls.is_empty();
                let has_tool_results = !msg.tool_results.is_empty();

                if !has_tool_calls && !has_tool_results {
                    continue;
                }

                let mut parts: Vec<String> = Vec::new();

                // Synthesize text for tool calls (matches Pi reader's format).
                for tc in &msg.tool_calls {
                    parts.push(format!("[Tool: {}]", tc.name));
                }

                // Synthesize text for tool results.
                for tr in &msg.tool_results {
                    if tr.is_error {
                        parts.push(format!("[Tool Error] {}", tr.content));
                    } else {
                        parts.push(format!("[Tool Output] {}", tr.content));
                    }
                }

                if !parts.is_empty() {
                    msg.content = parts.join("\n");
                }
            }
        }

        // 8. Write to target provider, on the flat track.
        let written = target_provider.write_session(&canonical, &write_opts)?;
        info!(
            target_session_id = written.session_id,
            resume_command = written.resume_command,
            "session written"
        );
        // Surface any non-fatal writer warnings (e.g. the target session was
        // written but could not be registered in the provider's resume index).
        all_warnings.extend(written.warnings.iter().cloned());

        // 9. Read-back verification.
        if let Some(first_path) = written.paths.first() {
            match target_provider.read_session(first_path) {
                Ok(readback) => {
                    debug!(
                        readback_messages = readback.messages.len(),
                        original_messages = canonical.messages.len(),
                        "read-back verification"
                    );
                    if let Some(detail) = readback_mismatch_detail(&canonical, &readback) {
                        warn!(detail, "read-back verification failed");
                        let rollback_detail =
                            match rollback_written_session(target_provider.slug(), &written) {
                                Ok(()) => "rollback succeeded".to_string(),
                                Err(rollback_error) => {
                                    format!("rollback failed: {rollback_error}")
                                }
                            };
                        return Err(CasrError::VerifyFailed {
                            provider: target_provider.slug().to_string(),
                            written_paths: written.paths.clone(),
                            detail: format!("{detail}; {rollback_detail}"),
                        }
                        .into());
                    }
                }
                Err(e) => {
                    warn!(error = %e, "read-back verification failed");
                    let rollback_detail =
                        match rollback_written_session(target_provider.slug(), &written) {
                            Ok(()) => "rollback succeeded".to_string(),
                            Err(rollback_error) => {
                                format!("rollback failed: {rollback_error}")
                            }
                        };
                    return Err(CasrError::VerifyFailed {
                        provider: target_provider.slug().to_string(),
                        written_paths: written.paths.clone(),
                        detail: format!("unable to read written session: {e}; {rollback_detail}"),
                    }
                    .into());
                }
            }
        }

        self.record_write(
            selection.as_ref(),
            target_provider,
            &written,
            fidelity,
            &losses,
            &mut all_warnings,
        );

        Ok(ConversionResult {
            source_provider: resolved.provider.slug().to_string(),
            target_provider: target_provider.slug().to_string(),
            canonical_session: canonical,
            written: Some(written),
            warnings: all_warnings,
            fidelity,
            losses,
            // The flat read-back checks message text and roles, not fidelity: it
            // has no independent grade to offer, so there is none to report.
            verified_fidelity: None,
            source: selection,
        })
    }

    // -- the store ----------------------------------------------------------

    /// Ask the store which incarnation of this conversation is the best source
    /// for `target`, and read that one instead when it is not the one named.
    ///
    /// `None` means the conversion carries on with the session it was given,
    /// which is what `--no-store` always means and what every other answer here
    /// degrades to. **Nothing in here can fail a conversion.** A store that
    /// cannot be read, a record that cannot be ingested, a chosen source that
    /// will not parse — each is reported as a warning and then ignored, because
    /// every one of them describes a broken *cache*, and refusing to convert a
    /// session that converts fine today would make the store a liability.
    ///
    /// A `--dry-run` ingests nothing. It still *consults* the store, so the
    /// source it reports is the source a real run would read, but a command whose
    /// contract is "write nothing" may not create a record either.
    fn select_source<'a>(
        &'a self,
        target: &dyn Provider,
        resolved: &mut ResolvedSession<'a>,
        canonical: &mut CanonicalSession,
        opts: &ConvertOptions,
        warnings: &mut Vec<String>,
    ) -> Option<SourceSelection> {
        let store = self.store.as_ref()?;
        let named = SessionKey::new(resolved.provider.slug(), canonical.session_id.clone());

        let record = match store.find_by_session(&named) {
            Ok(Some(record)) => record,
            Ok(None) if opts.dry_run => {
                debug!(%named, "dry run: not ingesting an unknown origin");
                return None;
            }
            Ok(None) => {
                match store.ingest_origin(&named, &resolved.path, OriginPolicy::Reference) {
                    Ok(record) => record,
                    Err(err) => {
                        warnings.push(format!(
                            "The session store could not remember {named}, so this conversion \
                             cannot be used as a better source later: {err}"
                        ));
                        return None;
                    }
                }
            }
            Err(err) => {
                warnings.push(format!(
                    "The session store could not be consulted, so {named} was read as named: {err}"
                ));
                return None;
            }
        };

        let choice = store.best_source_for(&record, target, &self.registry);
        // A record the ranking cannot settle is the one case where the store has
        // to say something even though it changed nothing: it read what was named,
        // and work that is not in the result exists somewhere else. Silence here
        // is the defect, not the fallback.
        if choice.unmergeable() {
            warnings.push(format!(
                "The session store cannot merge two incarnations of one conversation, so it read \
                 {named} as named — {}",
                choice.explain(Some(&named))
            ));
        }
        let Some(chosen) = choice.resolve(Some(&named)) else {
            // The record exists but the store can read none of it — including,
            // apparently, the file we just read successfully, which means its
            // recorded reference has gone stale. Report all three resolutions
            // rather than downgrade any of them silently, and carry on with what
            // was named.
            warnings.push(format!(
                "The session store has this conversation as record {} but {}; reading {named} as \
                 named.",
                record.id,
                choice.explain(None)
            ));
            return None;
        };

        if chosen.key == named {
            debug!(record = %record.id, %named, "the store agrees with the session that was named");
            return Some(SourceSelection {
                record_id: record.id,
                named,
                choice,
            });
        }

        let (key, path) = (chosen.key.clone(), chosen.path.clone());
        let Some(provider) = self.registry.find_by_slug(&key.provider) else {
            warnings.push(format!(
                "The session store offers {key} as a better source than {named}, but this build \
                 has no provider called '{}'; reading {named} as named.",
                key.provider
            ));
            return None;
        };
        let fresh = match provider.read_session(&path) {
            Ok(fresh) => fresh,
            Err(err) => {
                warnings.push(format!(
                    "The session store offers {key} as a better source than {named}, but it could \
                     not be read ({err}); reading {named} as named."
                ));
                return None;
            }
        };

        info!(
            record = %record.id,
            %named,
            chosen = %key,
            "the store chose a different source for this target"
        );
        *resolved = ResolvedSession { provider, path };
        *canonical = fresh;
        Some(SourceSelection {
            record_id: record.id,
            named,
            choice,
        })
    }

    /// Remember a conversion the store should know about.
    ///
    /// Also never fails the conversion: the target file is written and verified
    /// by the time this runs, and a store that could not be updated is a worse
    /// cache, not a worse conversion.
    ///
    /// A write with no paths — the same-provider short-circuit — records nothing.
    /// There is no new incarnation: the resume command points the agent back at
    /// bytes the record already holds.
    fn record_write(
        &self,
        selection: Option<&SourceSelection>,
        target: &dyn Provider,
        written: &WrittenSession,
        fidelity: Fidelity,
        losses: &[Loss],
        warnings: &mut Vec<String>,
    ) {
        let (Some(store), Some(selection)) = (self.store.as_ref(), selection) else {
            return;
        };
        let (Some(from), Some(path)) = (
            selection.chosen().map(|chosen| chosen.key.clone()),
            written.paths.first().cloned(),
        ) else {
            return;
        };
        let key = SessionKey::new(target.slug(), written.session_id.clone());
        let derived = DerivedWrite {
            key: key.clone(),
            path,
            from,
            fidelity,
            losses: losses.to_vec(),
        };
        match store.record_conversion(&selection.record_id, derived) {
            Ok(_) => debug!(record = %selection.record_id, %key, "recorded conversion"),
            Err(err) => warnings.push(format!(
                "The session written to {key} could not be recorded in the session store, so a \
                 later conversion will not know it is the same conversation: {err}"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Fidelity grading
// ---------------------------------------------------------------------------

/// The source session's structured IR, when it has one.
///
/// A structured reader that errors is treated as an absent reader rather than
/// as a failed conversion. The flat path does not need the IR, so refusing to
/// convert a session that the flat path handles fine would be a regression
/// introduced by the better track — exactly backwards.
///
/// It is *not* treated as an absent reader for grading, and the difference is
/// the whole reason this returns a `Result`. `Err` and `Ok(None)` used to be
/// the same `None`, and [`flat_fidelity`] reads `None` as "this provider never
/// had a structured reader, so nothing here is entitled to claim it lost
/// anything" — the benign [`Fidelity::ConversationOnly`] baseline. Applied to a
/// Codex rollout the reader choked on, that baseline is a claim nobody checked:
/// a sealed compaction the flat projection is about to delete would be graded
/// as an intact conversation. A provider that *has* a structured reader and
/// whose structured reader failed knows less about the file than one that never
/// had one, and the grade has to say so.
///
/// # Why the only skip here is the source's own capability
///
/// [`Provider::supports_structured_read`] is asked first so that the nineteen
/// providers with no structured reader are not called at all. That is the whole
/// of the legitimate skip, and it is exactly equivalent to calling them: the
/// trait default returns `Ok(None)` without opening the file, so the two paths
/// produce the same `None` for the same providers.
///
/// The larger-looking skip — "the target has no structured writer, so do not
/// parse" — is not available, and the reason is the second consumer. This IR
/// feeds [`flat_fidelity`] as well as [`Provider::write_session_ir`], and a
/// source whose structured read reveals a sealed compaction earns
/// [`Fidelity::HistoryIncomplete`] on the flat path. Skip the parse and that
/// session reports [`Fidelity::ConversationOnly`] instead: one saved parse
/// bought with a grade that no longer describes the file. A faster wrong answer
/// is not an optimization.
fn read_source_ir(provider: &dyn Provider, path: &Path) -> Result<Option<SessionIr>, String> {
    if !provider.supports_structured_read() {
        debug!(
            provider = provider.slug(),
            "no structured reader; grading and writing on the flat track"
        );
        return Ok(None);
    }
    match provider.read_session_ir(path) {
        Ok(ir) => Ok(ir),
        Err(error) => {
            warn!(
                provider = provider.slug(),
                path = %path.display(),
                %error,
                "structured read failed; grading and writing on the flat track"
            );
            Err(format!(
                "The {} structured reader could not parse {}, so this conversion ran on the \
                 flat projection without ever seeing the session's structure: {error}",
                provider.slug(),
                path.display(),
            ))
        }
    }
}

/// Read a structured write back off disk and compare it against the IR that
/// produced it.
///
/// Returns the warnings to surface, or the detail of a verification failure.
///
/// # What a mismatch does, and why
///
/// A mismatch is *not* treated as a write failure with a different message, and
/// it is emphatically not a reason to quietly lower
/// [`ConversionResult::fidelity`]. Three outcomes, by what the comparison
/// actually found:
///
/// - **Damage** — content missing that [`crate::ir::Capsule::fits`] did not
///   predict, or sealed bytes it forbade that crossed anyway. The written file
///   is rolled back and the conversion fails with [`CasrError::VerifyFailed`],
///   exactly as the flat track behaves. That error already says "this is a bug
///   in casr", which is what it is: the file on disk is not the session, and a
///   resume from it would hand the model an incomplete history with nothing to
///   show that anything was missing. Returning it as a *success* with a worse
///   grade would bury a writer bug inside the vocabulary reserved for honest
///   vendor-boundary losses, where nobody would ever go looking for it.
/// - **A grade the file does not support** — the comparator's independently
///   derived [`Fidelity`] is worse than the writer's claim. The bytes are fine,
///   so nothing is rolled back; the disagreement is surfaced as a warning
///   naming both grades. The writer's grade is still the one *reported*, because
///   silently substituting the comparator's would hide the disagreement just as
///   effectively as ignoring it — but the comparator's grade is returned
///   alongside it, and [`ConversionResult::effective_fidelity`] is what any
///   decision has to key on. Reporting the writer's number and then *acting* on
///   it are different things: the launch refusal used to read the claim, so a
///   writer that under-reported was launched over a hole the comparator had
///   already proved was there. Since the structured writers derive their grade
///   by folding their own loss list, a comparator grade worse than the claim is
///   a writer whose loss list is incomplete; that is a defect to surface, not to
///   resolve by picking a side.
/// - **Unverifiable** — the target has a structured writer but no structured
///   reader, or the vendor of its sealed formats is unknown to this version. No
///   check was possible, so the conversion is not failed and the skip is stated
///   in a warning rather than passing silently for a check that never ran.
///
/// # Why the budget is compared against, not ignored
///
/// The oracle is "does the file hold the session that was asked for", and a
/// `--max-context-tokens` conversion asks for the budgeted session. So `budget`
/// is applied to the source's replay here too, through the same
/// [`crate::budget::ContextBudget::apply`] the writer used, and the comparison
/// runs against that. Comparing the *un*budgeted replay instead would report
/// every trimmed turn as content that went missing with nothing predicting it —
/// which is the `Unexplained` bucket, so the first over-budget conversion would
/// roll itself back and fail as a writer bug. A verifier that fails the feature
/// it was extended for gets switched off, which is the same as not having one.
fn verify_structured_write(
    target: &dyn Provider,
    source_ir: &SessionIr,
    budget: &crate::budget::ContextBudget,
    written: &WrittenSession,
    claimed: Fidelity,
) -> Result<(Vec<String>, Option<Fidelity>), String> {
    let Some(path) = written.paths.first() else {
        return Ok((
            vec![format!(
                "Structured write to '{}' reported no output path, so it could not be verified.",
                target.slug()
            )],
            None,
        ));
    };

    let target_ir = match target.read_session_ir(path) {
        Ok(Some(ir)) => ir,
        Ok(None) => {
            return Ok((
                vec![format!(
                    "Wrote '{}' on the structured track but it has no structured reader, so the \
                     result could not be verified.",
                    target.slug()
                )],
                None,
            ));
        }
        Err(error) => {
            return Err(format!(
                "unable to read the written session back through the structured reader: {error}"
            ));
        }
    };

    // Keyed on the provider that was asked to write, not on anything inside the
    // file. `Origin::provider` records the endpoint a session was served by,
    // and 109 of the 591 rollouts in the corpus were served by a gateway under
    // its own name.
    let Some(vendor) = crate::compare::vendor_of(target.slug()) else {
        return Ok((
            vec![format!(
                "Wrote '{}' on the structured track but this version does not know which vendor's \
                 sealed formats it can replay, so the result could not be verified.",
                target.slug()
            )],
            None,
        ));
    };

    let budgeted = budget.apply(source_ir.model_visible());
    let report =
        crate::compare::compare_replays(&budgeted.as_events(), &target_ir.model_visible(), vendor);
    debug!(
        source_events = report.source_events,
        target_events = report.target_events,
        added_events = report.added_events,
        source_capsules = report.source_capsules,
        target_capsules = report.target_capsules,
        predicted = report.predicted.len(),
        degraded = report.degraded.len(),
        "structured read-back verified"
    );

    if !report.is_clean() {
        return Err(format!(
            "structured read-back found the written session is not the session that was \
             converted: {}",
            report.damage_detail()
        ));
    }

    let mut warnings = Vec::new();
    let observed = report.fidelity();
    if observed > claimed {
        warnings.push(format!(
            "The writer graded this conversion {claimed:?} but the written file only supports \
             {observed:?}. {}",
            report
                .predicted
                .iter()
                .chain(&report.degraded)
                .map(|loss| loss.note.clone())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if report.added_events > 0 {
        warnings.push(format!(
            "The written session holds {} model-visible event(s) the source did not, such as \
             the markers casr writes where a sealed history could not be carried.",
            report.added_events
        ));
    }
    Ok((warnings, Some(observed)))
}

/// Grade a conversion that went through the flat [`CanonicalSession`]
/// projection, and describe the loss when it is a loss of conversation.
///
/// # Why the baseline is `ConversationOnly` and not `TranscriptOnly`
///
/// `CanonicalSession` is not a transcript. Every message keeps its
/// [`MessageRole`], every [`crate::model::ToolCall`] keeps its name and its
/// JSON arguments, and every [`crate::model::ToolResult`] keeps its `is_error`
/// flag and its `call_id` link back to the call it answers. A reader handed
/// that can still tell who spoke and which tool produced which output, which
/// is more than "text survived; structure did not".
///
/// What the projection does lose is the protocol around that structure: Codex's
/// `function_call` / `custom_tool_call` distinction collapses to a name plus
/// arguments, compaction windows flatten into ordinary messages, model-visible
/// events and UI chrome merge into one list, and capsules are not carried at
/// all. That is precisely the line [`Fidelity::ConversationOnly`] draws —
/// "tool protocol downgraded, or compaction structure flattened, but every
/// piece of the conversation is still present". Grading it `TranscriptOnly`
/// would understate what the projection delivers.
///
/// # When the baseline does not hold
///
/// "Every piece of the conversation is still present" is a claim, not a
/// constant, and for one shape it is false. Codex hands compacted history back
/// as a sealed [`Body::SealedContext`] capsule; `CanonicalSession` carries no
/// capsules, so that history is deleted rather than degraded, and the grade
/// drops to [`Fidelity::HistoryIncomplete`].
///
/// Only a source with a structured reader can be checked this way. For the rest
/// `ir` is `Ok(None)` and the baseline stands — not because they lost nothing,
/// but because nothing here is entitled to claim they did.
///
/// `Err` is the third case, and it is not the second. The provider has a
/// structured reader, it ran, and it failed: the baseline's claim that "every
/// piece of the conversation is still present" is then not a modest reading of
/// a plain format but an unchecked assertion about a file this build could not
/// parse. Graded at the worst it could be, with the counts left at zero and the
/// note saying plainly that nothing was measured — the grade is a floor on how
/// bad this might be, not a finding.
/// The whole flat-track grade: the projection's own, plus the budget's.
///
/// Folded rather than accumulated separately: a [`Fidelity`] is the worst of its
/// losses, and a budget that deleted turns has to be able to make the grade
/// worse than the projection alone would.
///
/// Extracted so that the flat dry run and the flat write cannot report different
/// numbers. They used to: the dry run returned before the budget ran and graded
/// with [`flat_fidelity`] alone, so `--dry-run --max-context-tokens N` promised
/// a grade the same command without `--dry-run` would not produce, to precisely
/// the user asking whether that `N` was survivable.
fn flat_grade(
    source_ir: &Result<Option<SessionIr>, String>,
    target_slug: &str,
    budget_losses: Vec<Loss>,
) -> (Fidelity, Vec<Loss>) {
    let (mut fidelity, mut losses) = flat_fidelity(
        source_ir
            .as_ref()
            .map(Option::as_ref)
            .map_err(String::as_str),
        target_slug,
    );
    for loss in budget_losses {
        fidelity = fidelity.worse_of(loss.grade);
        losses.push(loss);
    }
    (fidelity, losses)
}

fn flat_fidelity(
    ir: Result<Option<&SessionIr>, &str>,
    target_slug: &str,
) -> (Fidelity, Vec<Loss>) {
    let baseline = Fidelity::ConversationOnly;

    let ir = match ir {
        Ok(Some(ir)) => ir,
        Ok(None) => return (baseline, Vec::new()),
        Err(detail) => {
            return (
                baseline.worse_of(Fidelity::HistoryIncomplete),
                vec![Loss {
                    kind: LossKind::Conversation,
                    events: 0,
                    capsules: 0,
                    bytes: 0,
                    grade: Fidelity::HistoryIncomplete,
                    note: format!(
                        "{detail} Whether it held a sealed compaction the flat projection \
                         deletes could not be determined, so this is graded as though it did.",
                    ),
                }],
            );
        }
    };

    let sealed: Vec<_> = ir
        .model_visible()
        .into_iter()
        .filter(|event| match &event.body {
            // Enumerated rather than wildcarded: a new body that also carries
            // conversation the flat projection cannot hold would otherwise be
            // graded `ConversationOnly` by default, which is the exact silent
            // degradation this function exists to catch.
            Body::SealedContext { .. } => true,
            Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::Compaction { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Rollback { .. }
            | Body::Abort {}
            | Body::Control { .. }
            | Body::Unknown { .. } => false,
        })
        .flat_map(|event| event.capsules.iter())
        .collect();
    let Some(first) = sealed.first() else {
        return (baseline, Vec::new());
    };

    let bytes: usize = sealed.iter().map(|capsule| capsule.sealed.len()).sum();
    (
        baseline.worse_of(Fidelity::HistoryIncomplete),
        vec![Loss {
            kind: LossKind::SealedContext,
            events: sealed.len(),
            capsules: sealed.len(),
            bytes,
            grade: Fidelity::HistoryIncomplete,
            note: format!(
                "{} compacted-history capsule(s) totalling {bytes} bytes are sealed to {} and \
                 cannot be written into a {target_slug} session. That is conversation, not \
                 reasoning: the resumed session is missing history and will not know it.",
                sealed.len(),
                first.kind.vendor(),
            ),
        }],
    )
}

// ---------------------------------------------------------------------------
// Context budget helpers
// ---------------------------------------------------------------------------

/// Rough token estimate (~4 chars/token) for one message, including tool I/O.
fn estimate_message_tokens(m: &CanonicalMessage) -> usize {
    let mut chars = m.content.len();
    for tc in &m.tool_calls {
        chars += tc.name.len() + tc.arguments.to_string().len();
    }
    for tr in &m.tool_results {
        chars += tr.content.len();
    }
    chars / 4 + 1
}

/// Trim a string to ~`max` chars, keeping head and tail with an elision marker.
/// Returns `None` if no truncation was needed.
///
/// Shared with [`crate::budget`], which truncates tool observations on the
/// structured track. One elision routine rather than two, so that the marker a
/// model reads is the same sentence whichever track produced the session.
pub(crate) fn elide_middle(s: &str, max: usize) -> Option<String> {
    if max == 0 {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return None;
    }
    let head_len = max.saturating_mul(2) / 3;
    let tail_len = max.saturating_sub(head_len);
    let omitted = chars.len() - head_len - tail_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    Some(format!("{head}\n…[casr: {omitted} chars elided]…\n{tail}"))
}

/// The tool ids that are already unpaired in `session`.
///
/// Taken before the budget runs, so that [`repair_tool_pairing`] can tell a pair
/// *it* broke from one that arrived broken.
fn unpaired_tool_ids(session: &CanonicalSession) -> HashSet<String> {
    let result_ids: HashSet<&str> = session
        .messages
        .iter()
        .flat_map(|m| m.tool_results.iter())
        .filter_map(|tr| tr.call_id.as_deref())
        .collect();
    let call_ids: HashSet<&str> = session
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .filter_map(|tc| tc.id.as_deref())
        .collect();

    let orphan_calls = call_ids.difference(&result_ids);
    let orphan_results = result_ids.difference(&call_ids);
    orphan_calls
        .chain(orphan_results)
        .map(|id| (*id).to_string())
        .collect()
}

/// Remove `tool_use` blocks that lack a matching `tool_result` (and vice
/// versa), then drop messages left with no content and no tool payloads.
///
/// The Anthropic API requires paired tool calls/results. After older turns are
/// dropped by the token budget, previously-paired tool_use/tool_result entries
/// can become orphaned; this function restores validity.
///
/// # Why `already_unpaired` is a parameter and not a recomputation
///
/// Real sessions end mid-tool-call: the user quits while the agent is waiting
/// for a command to return, and the last turn holds a call whose result never
/// arrived. Repairing by pairing alone deleted that call — and then, because
/// the message it lived in often has no text of its own, deleted the message —
/// on every cross-provider conversion, including ones where the budget removed
/// nothing at all. Nothing said so, and no [`Loss`] recorded it.
///
/// The ids that arrived unpaired are therefore left alone. They are already
/// what the source says; carrying them across is a faithful conversion, and
/// whether the target's API accepts a trailing unanswered call is the target
/// writer's problem to state rather than this function's to solve by deletion.
///
/// Returns the number of calls and results that were genuinely severed by the
/// budget, so the caller can report them.
fn repair_tool_pairing(
    session: &mut CanonicalSession,
    already_unpaired: &HashSet<String>,
) -> usize {
    let result_ids: HashSet<String> = session
        .messages
        .iter()
        .flat_map(|m| m.tool_results.iter())
        .filter_map(|tr| tr.call_id.clone())
        .collect();
    let call_ids: HashSet<String> = session
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .filter_map(|tc| tc.id.clone())
        .collect();

    let mut severed = 0usize;
    for m in &mut session.messages {
        m.tool_calls.retain(|tc| match tc.id.as_deref() {
            Some(id) => {
                let keep = result_ids.contains(id) || already_unpaired.contains(id);
                severed += usize::from(!keep);
                keep
            }
            None => true,
        });
        m.tool_results.retain(|tr| match tr.call_id.as_deref() {
            Some(id) => {
                let keep = call_ids.contains(id) || already_unpaired.contains(id);
                severed += usize::from(!keep);
                keep
            }
            None => true,
        });
    }
    session.messages.retain(|m| {
        !(m.content.trim().is_empty() && m.tool_calls.is_empty() && m.tool_results.is_empty())
    });
    severed
}

/// Fit a (cross-provider) session into a target-friendly context budget while
/// preserving its meaning. Steps, in order:
/// 1. Drop the source agent's hidden reasoning traces, if `--drop-reasoning`
///    asked. Never implied by either cap.
/// 2. Truncate oversized tool observations.
/// 3. Drop the oldest turns (excluding the first task message) if still over budget.
/// 4. Repair orphaned tool_use/tool_result pairs that result from the dropping.
///
/// Returns human-readable notes about what was elided — never silent — and the
/// [`Loss`] values behind them.
///
/// [`crate::budget::ContextBudget::UNLIMITED`] — what a caller who named no flag
/// gets — leaves every one of steps 1-3 switched off, so nothing is removed,
/// nothing is truncated, and both returned lists come back empty. Step 4 still
/// runs and is a no-op by construction: with nothing dropped, every id it can
/// see either still pairs or was already in `already_unpaired`.
///
/// # Why the losses exist as well as the notes
///
/// Step 3 deletes turns. It said so in a warning and stopped there, so
/// [`flat_fidelity`] never heard about it and a `--max-context-tokens`
/// conversion that removed the middle of a conversation still reported
/// [`Fidelity::ConversationOnly`] — a grade whose definition is "every piece of
/// the conversation is still present". `--launch` read that grade and started
/// the agent on a session with a hole in it. A warning is something a human may
/// read; a grade is what the refusal acts on, and the two have to agree.
///
/// The vocabulary is the same one [`crate::budget`] files on the structured
/// track, deliberately: eliding the middle of an observation leaves the event,
/// its link and its outcome, so it grades [`Fidelity::ConversationOnly`];
/// dropping a turn or severing a call from its result removes something the
/// model was shown, so it grades [`Fidelity::HistoryIncomplete`].
fn apply_context_budget(
    canonical: &mut CanonicalSession,
    budget: &crate::budget::ContextBudget,
) -> (Vec<String>, Vec<Loss>) {
    let crate::budget::ContextBudget {
        max_context_tokens: max_tokens,
        max_tool_output,
        keep_reasoning,
    } = *budget;
    let mut warnings = Vec::new();
    let mut losses = Vec::new();

    // Taken before anything is removed: an id that is already unpaired here is
    // the source's own shape, not damage this function did.
    let already_unpaired = unpaired_tool_ids(canonical);

    // 1. Drop source-agent reasoning traces (unusable by another agent).
    if !keep_reasoning {
        let before = canonical.messages.len();
        let mut bytes = 0usize;
        canonical.messages.retain(|m| {
            let is_reasoning = m.author.as_deref() == Some("reasoning");
            if is_reasoning {
                bytes += m.content.len();
            }
            !is_reasoning
        });
        let dropped = before - canonical.messages.len();
        if dropped > 0 {
            // Reached only when `--drop-reasoning` was passed — step 1 does not
            // run otherwise — so the note names the request rather than
            // advising a flag whose effect the caller is already getting.
            let note =
                format!("Dropped {dropped} source reasoning trace(s), as --drop-reasoning asked.");
            warnings.push(note.clone());
            losses.push(Loss {
                kind: LossKind::Reasoning,
                events: dropped,
                capsules: 0,
                bytes,
                grade: Fidelity::ContextNoReasoning,
                note,
            });
        }
    }

    // 2. Truncate oversized tool observations (the dominant byte source).
    if max_tool_output > 0 {
        let mut truncated = 0usize;
        let mut bytes = 0usize;
        for m in &mut canonical.messages {
            for tr in &mut m.tool_results {
                if let Some(short) = elide_middle(&tr.content, max_tool_output) {
                    bytes += tr.content.len().saturating_sub(short.len());
                    tr.content = short;
                    truncated += 1;
                }
            }
        }
        if truncated > 0 {
            let note = format!(
                "Truncated {truncated} oversized tool result(s) to ~{max_tool_output} chars each."
            );
            warnings.push(note.clone());
            losses.push(Loss {
                kind: LossKind::ToolProtocol,
                events: truncated,
                capsules: 0,
                bytes,
                grade: Fidelity::ConversationOnly,
                note,
            });
        }
    }

    // 3. Enforce the token budget by dropping the oldest turns, pinning the
    //    first (task) message and keeping the most recent history.
    if max_tokens > 0 && canonical.messages.len() > 1 {
        let total: usize = canonical.messages.iter().map(estimate_message_tokens).sum();
        if total > max_tokens {
            let pinned = estimate_message_tokens(&canonical.messages[0]);
            let mut budget_left = max_tokens.saturating_sub(pinned);
            let mut keep_from = canonical.messages.len();
            for i in (1..canonical.messages.len()).rev() {
                let t = estimate_message_tokens(&canonical.messages[i]);
                if t > budget_left {
                    break;
                }
                budget_left -= t;
                keep_from = i;
            }
            if keep_from > 1 {
                let dropped = keep_from - 1;
                let bytes: usize = canonical.messages[1..keep_from]
                    .iter()
                    .map(|m| m.content.len())
                    .sum();
                let tail = canonical.messages.split_off(keep_from);
                canonical.messages.truncate(1);
                canonical.messages.extend(tail);
                let note = format!(
                    "Context budget (~{max_tokens} tokens) exceeded; dropped {dropped} older \
turn(s) between the task and the most recent history."
                );
                warnings.push(note.clone());
                losses.push(Loss {
                    kind: LossKind::Conversation,
                    events: dropped,
                    capsules: 0,
                    bytes,
                    grade: Fidelity::HistoryIncomplete,
                    note,
                });
            }
        }
    }

    // 4. Re-pair tool calls/results and drop now-empty messages.
    let severed = repair_tool_pairing(canonical, &already_unpaired);
    if severed > 0 {
        let note = format!(
            "Dropped {severed} tool call(s)/result(s) whose other half was removed by the \
             context budget."
        );
        warnings.push(note.clone());
        losses.push(Loss {
            kind: LossKind::ToolProtocol,
            events: severed,
            capsules: 0,
            bytes: 0,
            grade: Fidelity::HistoryIncomplete,
            note,
        });
    }
    reindex_messages(&mut canonical.messages);

    (warnings, losses)
}

/// Coarse role bucket used for read-back verification.
///
/// Some target formats (notably Claude Code JSONL) don't distinguish between
/// User, System, Tool, and Other roles — they all become `"user"` entries.
/// When we read back the written session the roles come back as `User`,
/// causing a spurious mismatch against the original `System`/`Tool`/`Other`.
///
/// This function maps every role to a small set of equivalence classes so the
/// verification comparison is tolerant of this expected lossy round-trip.
fn readback_role_bucket(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::Assistant => "assistant",
        // Everything else collapses into the "user" bucket because that is
        // the only non-assistant entry type Claude Code (and similar formats)
        // can represent.
        MessageRole::User | MessageRole::System | MessageRole::Tool | MessageRole::Other(_) => {
            "user"
        }
    }
}

fn readback_mismatch_detail(
    canonical: &CanonicalSession,
    readback: &CanonicalSession,
) -> Option<String> {
    if readback.messages.len() != canonical.messages.len() {
        return Some(format!(
            "message count mismatch: wrote {} messages, read back {}",
            canonical.messages.len(),
            readback.messages.len()
        ));
    }

    for (i, (orig, rb)) in canonical
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        if readback_role_bucket(&orig.role) != readback_role_bucket(&rb.role) {
            return Some(format!(
                "message role mismatch at idx {i}: wrote {:?}, read back {:?}",
                orig.role, rb.role
            ));
        }
        if orig.content != rb.content {
            return Some(format!(
                "message content mismatch at idx {i}: wrote {} bytes, read back {} bytes",
                orig.content.len(),
                rb.content.len()
            ));
        }
    }

    None
}

/// Remove a file that may already be gone.
fn remove_if_present(path: &Path, provider_slug: &str, what: &str) -> Result<(), CasrError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CasrError::SessionWriteError {
            path: path.to_path_buf(),
            provider: provider_slug.to_string(),
            detail: format!("{what}: {error}"),
        }),
    }
}

/// Undo a write that failed verification: take back every file it produced and
/// put back every file it displaced.
///
/// # Why the pairing is carried rather than assumed
///
/// This used to restore [`WrittenSession::backups`]' single predecessor —
/// a bare `backup_path` — onto `paths[0]`, which assumed the one backup a write
/// took was a backup *of its first output*. Cline breaks that assumption
/// completely: its only backup is of `state/taskHistory.json`, the shared task
/// index, while `paths[0]` is `api_conversation_history.json`. The rollback
/// therefore deleted the new API history, moved the old global index into its
/// place, left the modified index installed, and reported that it had succeeded
/// — three files wrong, no error. Kiro broke it more quietly: it writes two or
/// three files under `--force` and could only ever hand back the first one's
/// backup, so a rollback left the others' predecessors sitting in `.bak` files
/// nothing would ever restore.
///
/// Each [`Displaced`] now names the file it restores, so neither provider has to
/// be special-cased and a future multi-file writer cannot reintroduce either
/// shape.
fn rollback_written_session(
    provider_slug: &str,
    written: &WrittenSession,
) -> Result<(), CasrError> {
    // Outputs first. A displaced file may also be one of them (the ordinary
    // `--force` case, where the write replaced the very file it backed up), and
    // removing after restoring would delete what was just put back.
    for path in &written.paths {
        remove_if_present(path, provider_slug, "failed to remove unverified output")?;
    }

    for displaced in &written.backups {
        warn!(
            backup = %displaced.backup.display(),
            target = %displaced.target.display(),
            "restoring backup after verification failure"
        );
        remove_if_present(
            &displaced.target,
            provider_slug,
            "failed to remove unverified output before restore",
        )?;
        std::fs::rename(&displaced.backup, &displaced.target).map_err(|error| {
            CasrError::SessionWriteError {
                path: displaced.target.clone(),
                provider: provider_slug.to_string(),
                detail: format!("failed to restore backup: {error}"),
            }
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic file writing
// ---------------------------------------------------------------------------

/// Outcome of a successful atomic write operation.
#[derive(Debug, Clone)]
pub struct AtomicWriteOutcome {
    /// Final destination path.
    pub target_path: PathBuf,
    /// Temp file used during write (already renamed away).
    pub temp_path: PathBuf,
    /// Path to the `.bak` backup of a pre-existing file (if `--force` was used).
    pub backup_path: Option<PathBuf>,
}

impl AtomicWriteOutcome {
    /// What this write displaced, if it displaced anything.
    ///
    /// The pair every caller needs and none of them should have to assemble:
    /// a writer that produces several files has several of these, and only the
    /// call that made each one knows which target its backup belongs to.
    pub fn displaced(&self) -> Option<crate::providers::Displaced> {
        self.backup_path
            .as_ref()
            .map(|backup| crate::providers::Displaced {
                target: self.target_path.clone(),
                backup: backup.clone(),
            })
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicWriteFailStage {
    BackupRename,
    TempFileCreate,
    WriteAll,
    Flush,
    SyncAll,
    FinalRename,
}

#[cfg(test)]
thread_local! {
    static ATOMIC_WRITE_FAIL_STAGE: std::cell::Cell<Option<AtomicWriteFailStage>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn set_atomic_write_fail_stage(stage: Option<AtomicWriteFailStage>) {
    ATOMIC_WRITE_FAIL_STAGE.with(|slot| slot.set(stage));
}

#[cfg(test)]
fn maybe_inject_atomic_write_failure(stage: AtomicWriteFailStage) -> std::io::Result<()> {
    let injected = ATOMIC_WRITE_FAIL_STAGE.with(|slot| slot.get() == Some(stage));
    if injected {
        return Err(std::io::Error::other(format!(
            "injected atomic_write failure at stage {stage:?}"
        )));
    }
    Ok(())
}

/// Write `content` atomically to `target_path` using temp-then-rename.
///
/// Guarantees: either the old target remains intact, or the new target is
/// fully written and fsynced. Never leaves partial writes.
///
/// Returns `AtomicWriteOutcome` on success, or:
/// - [`CasrError::SessionConflict`] if target exists and `force` is false.
/// - [`CasrError::SessionWriteError`] on I/O failures.
///
/// # Why the original is never unlinked
///
/// A forced write used to begin by renaming the target out of the way, onto a
/// name chosen by testing each candidate for existence. Two things fell out of
/// that, and both destroy the file this program exists to protect:
///
/// - **The name was chosen, then used.** Two forced conversions aimed at one
///   target both saw no `session.jsonl.bak` and both picked it. The first
///   installed its output; the second renamed *that* over the backup. The
///   original was gone, and the only copy of it had been overwritten by a file
///   that was still on disk anyway.
/// - **The target stopped existing.** Between the two renames there is no file
///   at the path the agent reads. A crash, a full disk, or a signal in that
///   window leaves the session missing rather than merely stale.
///
/// So the order is inverted: the new content is written and fsynced *first*,
/// then the original is preserved by [`preserve_original`] without being
/// unlinked — a hard link, whose failure with `AlreadyExists` is itself the
/// name reservation, so two writers cannot agree on one backup name. Only then
/// does one `rename` replace the target, which is atomic: readers see the old
/// file or the new one and never neither. The directory is fsynced afterwards
/// so the replacement survives the crash the temp file's own `sync_all` was
/// already guarding against.
pub fn atomic_write(
    target_path: &Path,
    content: &[u8],
    force: bool,
    provider_slug: &str,
) -> Result<AtomicWriteOutcome, CasrError> {
    use std::io::Write;

    // 1. Create parent directories.
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: provider_slug.to_string(),
            detail: format!("failed to create parent directories: {e}"),
        })?;
    }

    // 2. Refuse an existing target unless forced. Nothing is moved here: the
    //    check is a check, and the file stays where the agent can read it.
    let displaces_existing = target_path.exists();
    if displaces_existing && !force {
        return Err(CasrError::SessionConflict {
            session_id: String::new(),
            existing_path: target_path.to_path_buf(),
        });
    }

    // 3. Write to temp file in the same directory.
    let temp_name = format!(".casr-tmp-{}", uuid::Uuid::new_v4().as_hyphenated());
    let temp_path = target_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(&temp_name);

    let write_result = (|| -> Result<(), std::io::Error> {
        #[cfg(test)]
        maybe_inject_atomic_write_failure(AtomicWriteFailStage::TempFileCreate)?;
        let mut file = std::fs::File::create(&temp_path)?;
        #[cfg(test)]
        maybe_inject_atomic_write_failure(AtomicWriteFailStage::WriteAll)?;
        file.write_all(content)?;
        #[cfg(test)]
        maybe_inject_atomic_write_failure(AtomicWriteFailStage::Flush)?;
        file.flush()?;
        #[cfg(test)]
        maybe_inject_atomic_write_failure(AtomicWriteFailStage::SyncAll)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // The target was never touched, so cleaning up the temp file is the
        // whole of the recovery.
        let _ = std::fs::remove_file(&temp_path);
        return Err(CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: provider_slug.to_string(),
            detail: format!("failed to write temp file: {e}"),
        });
    }

    // 4. Preserve the original alongside itself, without unlinking it.
    let backup_path = if displaces_existing {
        let reserved = (|| -> std::io::Result<Option<PathBuf>> {
            #[cfg(test)]
            maybe_inject_atomic_write_failure(AtomicWriteFailStage::BackupRename)?;
            preserve_original(target_path)
        })();
        match reserved {
            Ok(bak) => {
                if let Some(bak) = &bak {
                    debug!(
                        target = %target_path.display(),
                        backup = %bak.display(),
                        "preserved existing file"
                    );
                }
                bak
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(CasrError::SessionWriteError {
                    path: target_path.to_path_buf(),
                    provider: provider_slug.to_string(),
                    detail: format!("failed to create backup: {e}"),
                });
            }
        }
    } else {
        None
    };

    // 5. One rename installs the new content. The target held the old file
    //    until this instant and holds the new one after it.
    let rename_result = (|| -> std::io::Result<()> {
        #[cfg(test)]
        maybe_inject_atomic_write_failure(AtomicWriteFailStage::FinalRename)?;
        std::fs::rename(&temp_path, target_path)
    })();

    if let Err(e) = rename_result {
        let _ = std::fs::remove_file(&temp_path);
        // The backup is a second name for the file still at `target_path`, so
        // dropping that name is the undo — and leaving it behind would strand a
        // `.bak` for a write that never happened.
        if let Some(ref bak) = backup_path {
            let _ = std::fs::remove_file(bak);
        }
        return Err(CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: provider_slug.to_string(),
            detail: format!("failed to rename temp file to target: {e}"),
        });
    }

    // 6. Make the replacement itself durable. `sync_all` on the temp file
    //    persisted the bytes; the rename that gave them the target's name lives
    //    in the directory.
    if let Some(parent) = target_path.parent() {
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    info!(target = %target_path.display(), "atomic write complete");

    Ok(AtomicWriteOutcome {
        target_path: target_path.to_path_buf(),
        temp_path,
        backup_path,
    })
}

/// Restore a backup after a verification failure.
///
/// Removes the broken target and renames the backup back into place.
pub fn restore_backup(outcome: &AtomicWriteOutcome, provider_slug: &str) -> Result<(), CasrError> {
    if let Some(ref bak) = outcome.backup_path {
        warn!(
            backup = %bak.display(),
            target = %outcome.target_path.display(),
            "restoring backup after verification failure"
        );
        let _ = std::fs::remove_file(&outcome.target_path);
        std::fs::rename(bak, &outcome.target_path).map_err(|e| CasrError::SessionWriteError {
            path: outcome.target_path.clone(),
            provider: provider_slug.to_string(),
            detail: format!("failed to restore backup: {e}"),
        })?;
    } else {
        // No backup: just remove the broken target.
        let _ = std::fs::remove_file(&outcome.target_path);
    }
    Ok(())
}

/// The backup names for `target`, in the order they are tried: `.bak`, then
/// `.bak.1` … `.bak.99`, then one with a random suffix that cannot collide.
fn backup_candidates(target: &Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    let mut base = target.file_name().unwrap_or_default().to_os_string();
    base.push(".bak");
    (0..=100).map(move |i| {
        let mut name = base.clone();
        match i {
            0 => {}
            100 => name.push(format!(".{}", uuid::Uuid::new_v4().as_hyphenated())),
            n => name.push(format!(".{n}")),
        }
        target.with_file_name(name)
    })
}

/// Give `target`'s current contents a second name, without taking away the one
/// they already have.
///
/// Returns the name that was taken, or `None` if `target` disappeared before it
/// could be preserved — which is not a failure to write, only a race with
/// something else deleting a file we were about to replace anyway.
///
/// # Why a link rather than a rename or a copy
///
/// The reservation has to be atomic or it is not a reservation: "does this name
/// exist yet" answered before the name is used is a question two concurrent
/// forced conversions both answer `no`. `link(2)` fails with `EEXIST` instead,
/// so *taking* the name and *checking* it are one operation and the loser simply
/// moves to the next candidate. It is also O(1) — the largest rollout in the
/// corpus is 281 MiB — and it leaves the original in place, so there is no
/// window in which the session has no file.
///
/// A copy is the fallback for filesystems with no hard links, where the name is
/// reserved by an exclusive create instead. It is slower and it can be
/// interrupted, but an interrupted copy loses only the backup: the original is
/// still at `target`, because nothing has moved it.
fn preserve_original(target: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::io::ErrorKind;

    let mut links_unsupported = false;
    for candidate in backup_candidates(target) {
        if !links_unsupported {
            match std::fs::hard_link(target, &candidate) {
                Ok(()) => return Ok(Some(candidate)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                // EPERM/EMLINK/EXDEV and friends: this filesystem will not link.
                Err(_) => links_unsupported = true,
            }
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        match std::fs::copy(target, &candidate) {
            Ok(_) => return Ok(Some(candidate)),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let _ = std::fs::remove_file(&candidate);
                return Ok(None);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&candidate);
                return Err(error);
            }
        }
    }
    Err(std::io::Error::other(format!(
        "no free backup name for {}",
        target.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::ContextBudget;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn sample_message(idx: usize, role: MessageRole, content: &str) -> CanonicalMessage {
        CanonicalMessage {
            idx,
            role,
            content: content.to_string(),
            timestamp: Some(1_700_000_000_000 + idx as i64),
            author: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            extra: serde_json::Value::Null,
        }
    }

    fn sample_session() -> CanonicalSession {
        CanonicalSession {
            session_id: "src-123".to_string(),
            provider_slug: "codex".to_string(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            title: Some("Example".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            messages: vec![
                sample_message(
                    0,
                    MessageRole::User,
                    "Investigate parser behavior in providers/codex.rs",
                ),
                sample_message(
                    1,
                    MessageRole::Assistant,
                    "I found a mismatch in response_item handling; I will patch it.",
                ),
                sample_message(
                    2,
                    MessageRole::User,
                    "Please also verify resume command compatibility.",
                ),
            ],
            metadata: serde_json::Value::Null,
            source_path: PathBuf::from("/tmp/source.jsonl"),
            model_name: Some("gpt-5-codex".to_string()),
        }
    }

    #[test]
    fn enrich_prepends_marked_synthetic_messages() {
        let mut session = sample_session();
        let original_len = session.messages.len();

        let inserted = prepend_enrichment_messages(&mut session, "codex", "claude-code", "src-123");

        assert_eq!(inserted, 2);
        assert_eq!(session.messages.len(), original_len + 2);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[1].role, MessageRole::System);
        assert!(
            session.messages[0]
                .content
                .contains("[casr synthetic context]")
        );
        assert!(
            session.messages[1]
                .content
                .contains("Recent conversation snapshot")
        );
        assert_eq!(
            session.messages[0]
                .extra
                .get("casr_enrichment")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            session.messages[1]
                .extra
                .get("enrichment_type")
                .and_then(|v| v.as_str()),
            Some("recent_summary")
        );

        for (idx, msg) in session.messages.iter().enumerate() {
            assert_eq!(msg.idx, idx);
        }
    }

    #[test]
    fn recent_summary_is_deterministic_and_compact() {
        let mut session = sample_session();
        session.messages.push(sample_message(
            3,
            MessageRole::Assistant,
            "   This    has  extra   spacing\nand line breaks that should compact cleanly.   ",
        ));

        let (count, summary) = build_recent_summary(&session, 2, 40);
        assert_eq!(count, 2);
        assert!(summary.contains("- user: Please also verify resume command"));
        assert!(summary.contains("- assistant: This has extra spacing"));
        assert!(summary.contains("..."));
    }

    struct FailStageReset;

    impl Drop for FailStageReset {
        fn drop(&mut self) {
            set_atomic_write_fail_stage(None);
        }
    }

    fn with_fail_stage(stage: AtomicWriteFailStage) -> FailStageReset {
        set_atomic_write_fail_stage(Some(stage));
        FailStageReset
    }

    fn count_temp_artifacts(dir: &Path) -> usize {
        fs::read_dir(dir)
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".casr-tmp-")
            })
            .count()
    }

    fn backup_artifacts_for(target: &Path) -> Vec<PathBuf> {
        let parent = target.parent().expect("target parent");
        let prefix = format!(
            "{}.bak",
            target
                .file_name()
                .expect("target file name")
                .to_string_lossy()
        );
        fs::read_dir(parent)
            .expect("read parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    #[test]
    fn atomic_write_conflict_without_force_returns_session_conflict() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let target = tmp.path().join("session.jsonl");
        fs::write(&target, "existing").expect("seed target");

        let err =
            atomic_write(&target, b"new content", false, "test").expect_err("should conflict");
        assert!(matches!(err, CasrError::SessionConflict { .. }));
        assert_eq!(
            fs::read_to_string(&target).expect("target should remain"),
            "existing"
        );
    }

    #[test]
    fn atomic_write_failure_matrix_restores_backup_and_cleans_temp_files() {
        for stage in [
            AtomicWriteFailStage::TempFileCreate,
            AtomicWriteFailStage::WriteAll,
            AtomicWriteFailStage::Flush,
            AtomicWriteFailStage::SyncAll,
            AtomicWriteFailStage::FinalRename,
        ] {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let target = tmp.path().join("session.jsonl");
            fs::write(&target, "original").expect("seed target");

            let _reset = with_fail_stage(stage);
            let err =
                atomic_write(&target, b"new content", true, "test").expect_err("expected failure");
            assert!(
                matches!(err, CasrError::SessionWriteError { .. }),
                "expected SessionWriteError for stage {stage:?}, got {err:?}"
            );

            assert_eq!(
                fs::read_to_string(&target).expect("target should be restored"),
                "original",
                "original content should be restored for stage {stage:?}"
            );
            assert_eq!(
                count_temp_artifacts(tmp.path()),
                0,
                "no temp artifacts should remain for stage {stage:?}"
            );
            assert!(
                backup_artifacts_for(&target).is_empty(),
                "backup artifacts should not remain for stage {stage:?}"
            );
        }
    }

    #[test]
    fn atomic_write_backup_creation_failure_preserves_original_target() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let target = tmp.path().join("session.jsonl");
        fs::write(&target, "original").expect("seed target");

        let _reset = with_fail_stage(AtomicWriteFailStage::BackupRename);
        let err =
            atomic_write(&target, b"new content", true, "test").expect_err("expected failure");
        let CasrError::SessionWriteError { detail, .. } = err else {
            panic!("expected SessionWriteError, got {err:?}");
        };
        assert!(
            detail.contains("failed to create backup"),
            "unexpected detail: {detail}"
        );

        assert_eq!(
            fs::read_to_string(&target).expect("target should remain"),
            "original"
        );
        assert_eq!(count_temp_artifacts(tmp.path()), 0);
        assert!(backup_artifacts_for(&target).is_empty());
    }

    #[test]
    fn atomic_write_success_force_creates_backup_and_restore_backup_recovers_original() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let target = tmp.path().join("session.jsonl");
        fs::write(&target, "original").expect("seed target");

        let outcome = atomic_write(&target, b"new content", true, "test")
            .expect("force write should succeed");
        assert_eq!(
            fs::read_to_string(&target).expect("target should contain new content"),
            "new content"
        );
        assert!(
            !outcome.temp_path.exists(),
            "temp file should be renamed away"
        );

        let backup = outcome.backup_path.as_ref().expect("backup should exist");
        assert_eq!(
            fs::read_to_string(backup).expect("backup should contain original"),
            "original"
        );

        restore_backup(&outcome, "test").expect("restore should succeed");
        assert_eq!(
            fs::read_to_string(&target).expect("target should be restored"),
            "original"
        );
        assert!(!backup.exists(), "backup should be consumed during restore");
    }

    #[test]
    fn restore_backup_without_backup_removes_target() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let target = tmp.path().join("session.jsonl");

        let outcome = atomic_write(&target, b"fresh content", false, "test")
            .expect("initial write should succeed");
        assert!(target.exists(), "target should exist after write");
        assert!(outcome.backup_path.is_none(), "no backup expected");

        restore_backup(&outcome, "test").expect("restore should succeed without backup");
        assert!(
            !target.exists(),
            "target should be removed when no backup is available"
        );
    }

    // -----------------------------------------------------------------------
    // Context budget regression tests
    // -----------------------------------------------------------------------

    fn budget_msg(role: MessageRole, content: &str) -> CanonicalMessage {
        CanonicalMessage {
            idx: 0,
            role,
            content: content.to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }
    }

    fn budget_session(messages: Vec<CanonicalMessage>) -> CanonicalSession {
        CanonicalSession {
            session_id: "s".into(),
            provider_slug: "codex".into(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages,
            metadata: serde_json::Value::Null,
            source_path: PathBuf::from("/tmp/x"),
            model_name: None,
        }
    }

    #[test]
    fn budget_elide_middle_only_truncates_long_strings() {
        assert!(elide_middle("short", 100).is_none(), "no truncation needed");
        let long = "x".repeat(1000);
        let out = elide_middle(&long, 100).expect("should truncate");
        assert!(out.chars().count() < 250, "output should be much shorter");
        assert!(out.contains("elided"), "should contain elision marker");
    }

    #[test]
    fn budget_drops_reasoning_and_truncates_tool_output() {
        use crate::model::{ToolCall, ToolResult};

        let mut reasoning = budget_msg(MessageRole::Assistant, "secret thoughts");
        reasoning.author = Some("reasoning".into());

        let mut call = budget_msg(MessageRole::Assistant, "run it");
        call.tool_calls.push(ToolCall {
            id: Some("c1".into()),
            name: "Bash".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        });

        let mut tool = budget_msg(MessageRole::Tool, "");
        tool.tool_results.push(ToolResult {
            call_id: Some("c1".into()),
            content: "y".repeat(50_000),
            is_error: false,
        });

        let task = budget_msg(MessageRole::User, "task");
        let mut s = budget_session(vec![task, call, tool, reasoning]);

        let (warns, _) = apply_context_budget(
            &mut s,
            &ContextBudget {
                max_context_tokens: 0,
                max_tool_output: 4_000,
                keep_reasoning: false,
            },
        );

        // Reasoning was dropped.
        assert!(
            !s.messages
                .iter()
                .any(|m| m.author.as_deref() == Some("reasoning")),
            "reasoning trace should be gone"
        );

        // Tool output was truncated.
        let tr_content = &s
            .messages
            .iter()
            .find(|m| !m.tool_results.is_empty())
            .expect("tool result message kept")
            .tool_results[0]
            .content;
        assert!(
            tr_content.chars().count() < 5000,
            "tool output should be truncated"
        );

        // Warnings were emitted.
        assert!(
            warns.iter().any(|w| w.contains("reasoning")),
            "should warn about dropped reasoning"
        );
        assert!(
            warns.iter().any(|w| w.contains("Truncated")),
            "should warn about truncated tool output"
        );
    }

    #[test]
    fn budget_token_cap_drops_oldest_keeps_task_and_recent() {
        let mut msgs = vec![budget_msg(MessageRole::User, "the original task")];
        for i in 0..50 {
            let role = if i % 2 == 0 {
                MessageRole::Assistant
            } else {
                MessageRole::User
            };
            msgs.push(budget_msg(role, &"word ".repeat(500)));
        }
        msgs.push(budget_msg(MessageRole::Assistant, "FINAL RECENT MESSAGE"));
        let before = msgs.len();
        let mut s = budget_session(msgs);

        let (warns, _) = apply_context_budget(
            &mut s,
            &ContextBudget {
                max_context_tokens: 2_000,
                ..ContextBudget::UNLIMITED
            },
        );

        assert!(s.messages.len() < before, "older turns should be dropped");
        assert_eq!(
            s.messages.first().unwrap().content,
            "the original task",
            "first (task) message must be pinned"
        );
        assert_eq!(
            s.messages.last().unwrap().content,
            "FINAL RECENT MESSAGE",
            "most recent message must be retained"
        );
        assert!(
            warns.iter().any(|w| w.contains("Context budget")),
            "should emit context-budget warning"
        );
    }

    #[test]
    fn budget_repairs_orphaned_tool_use_after_dropping() {
        use crate::model::ToolCall;

        let mut call = budget_msg(MessageRole::Assistant, "");
        call.tool_calls.push(ToolCall {
            id: Some("orphan".into()),
            name: "X".into(),
            arguments: serde_json::Value::Null,
        });
        // No matching tool_result — the tool call arrived orphaned, which is
        // what a session that ended mid-command looks like. It is the source's
        // own shape and nothing here removed anything, so nothing here may
        // delete it: the previous behaviour dropped the call and then the whole
        // turn, on every conversion, with no warning and no `Loss`.
        let mut s = budget_session(vec![budget_msg(MessageRole::User, "hi"), call]);

        let (warnings, losses) = apply_context_budget(&mut s, &ContextBudget::UNLIMITED);

        assert_eq!(
            s.messages.len(),
            2,
            "a turn the budget did not touch must survive it"
        );
        assert_eq!(
            s.messages[1].tool_calls.len(),
            1,
            "a call that was never answered is not damage to repair"
        );
        assert!(warnings.is_empty(), "nothing happened: {warnings:?}");
        assert!(losses.is_empty(), "nothing happened: {losses:?}");
    }

    #[test]
    fn budget_reports_the_pairs_it_severs() {
        use crate::model::{ToolCall, ToolResult};

        let mut call = budget_msg(MessageRole::Assistant, &"word ".repeat(2000));
        call.tool_calls.push(ToolCall {
            id: Some("paired".into()),
            name: "X".into(),
            arguments: serde_json::Value::Null,
        });
        let mut answer = budget_msg(MessageRole::Tool, "ok");
        answer.tool_results.push(ToolResult {
            call_id: Some("paired".into()),
            content: "ok".into(),
            is_error: false,
        });
        let mut s = budget_session(vec![
            budget_msg(MessageRole::User, "the original task"),
            call,
            answer,
            budget_msg(MessageRole::User, "and now this"),
        ]);

        // Small enough to drop the expensive call, large enough to keep the
        // cheap result that answered it.
        let (_warnings, losses) = apply_context_budget(
            &mut s,
            &ContextBudget {
                max_context_tokens: 300,
                ..ContextBudget::UNLIMITED
            },
        );

        assert!(
            losses.iter().any(|loss| loss.kind == LossKind::Conversation
                && loss.grade == Fidelity::HistoryIncomplete),
            "a dropped turn is a hole in the conversation: {losses:?}"
        );
        assert!(
            losses.iter().any(|loss| loss.kind == LossKind::ToolProtocol
                && loss.grade == Fidelity::HistoryIncomplete),
            "and severing its result from it is a second one: {losses:?}"
        );
    }

    /// A budget nobody asked for removes nothing — on a session that a budget
    /// somebody asked for would visibly cut.
    ///
    /// The caps shipped as clap defaults: `--max-context-tokens 200000
    /// --max-tool-output 4000`, with reasoning dropped unless `--keep-reasoning`
    /// was passed. So every conversion carried a budget, and on the local corpus
    /// that was not theoretical — 747 of 833 sessions were trimmed on this
    /// track, ten of them losing 12,202 whole messages, which grades
    /// [`Fidelity::HistoryIncomplete`] and makes `--launch` refuse. The fixture
    /// below is that shape, and the two halves of this test are the two things
    /// the change has to keep true at once: absence removes nothing, and asking
    /// still works and is still counted.
    #[test]
    fn an_unrequested_budget_removes_nothing() {
        use crate::model::ToolResult;

        let mut oversized = budget_msg(MessageRole::Tool, "");
        oversized.tool_results.push(ToolResult {
            call_id: None,
            content: "y".repeat(50_000),
            is_error: false,
        });
        let mut reasoning = budget_msg(MessageRole::Assistant, "secret thoughts");
        reasoning.author = Some("reasoning".into());

        let mut messages = vec![budget_msg(MessageRole::User, "the original task")];
        for _ in 0..40 {
            messages.push(budget_msg(MessageRole::Assistant, &"word ".repeat(5_000)));
        }
        messages.push(oversized);
        messages.push(reasoning);
        messages.push(budget_msg(MessageRole::User, "and now this"));

        assert!(
            ConvertOptions::default().budget.is_unlimited(),
            "a conversion nobody constrained carries the whole session"
        );

        let mut untouched = budget_session(messages.clone());
        let (warnings, losses) =
            apply_context_budget(&mut untouched, &ConvertOptions::default().budget);
        // Renumbering is not removal, and it happens to every conversion.
        let mut expected = messages.clone();
        reindex_messages(&mut expected);
        assert_eq!(
            untouched.messages, expected,
            "an absent budget may not drop, truncate or reorder a single message"
        );
        assert!(warnings.is_empty(), "and reports nothing: {warnings:?}");
        assert!(losses.is_empty(), "and loses nothing: {losses:?}");

        let mut asked = budget_session(messages);
        let (warnings, losses) = apply_context_budget(
            &mut asked,
            &ContextBudget {
                max_context_tokens: 200_000,
                max_tool_output: 4_000,
                keep_reasoning: false,
            },
        );
        assert!(
            asked.messages.len() < untouched.messages.len(),
            "the same caps, asked for, still cut this session"
        );
        assert!(!warnings.is_empty(), "and still say so");
        assert!(
            losses.iter().any(|loss| loss.kind == LossKind::Conversation
                && loss.grade == Fidelity::HistoryIncomplete),
            "and a dropped turn still grades the conversion down: {losses:?}"
        );
        assert!(
            losses.iter().any(|loss| loss.kind == LossKind::Reasoning
                && loss.grade == Fidelity::ContextNoReasoning),
            "and dropped reasoning is still counted: {losses:?}"
        );
    }
}
