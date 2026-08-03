# Extending the high-fidelity track

Two things are expected to keep changing: the IR, and the set of providers on
the structured track. This document is about making both cheap, and — more
importantly — about making a mistake in either one *fail loudly* rather than
produce a plausible, wrong session.

That distinction is the whole point. Every real defect found in this crate so
far has been silent:

| Defect | How it hid |
|---|---|
| 4,325 sealed compaction items, 87.6 MB, replayed as empty assistant messages | `filter_map` returned `None`, and `None` is indistinguishable from "nothing was there" |
| `Vec<Event>` nested inside `Body::Compaction` | those events were outside every traversal, so `CaptureReport` counted zero loss while losing everything |
| 4,562 `encrypted_content` blocks inside Codex `agent_message` | same `filter_map`, different site |
| 100% of Codex tool results reported success | no evidence either way, and "no evidence" defaulted to success |
| Rollback rule never fired on any of 714 real rollbacks | the rule sat behind a visibility gate that Codex's own control events never pass |

None of these failed a test. Each was found by counting something against the
corpus. So the design rule is not "be flexible" — it is **make the compiler or
a corpus count notice before a human has to.**

## 1. The IR is a cache, not a record

`SessionIr` derives `Serialize`/`Deserialize` and it is tempting to persist it.
Do not make it a source of truth.

The source of truth is the provider's original bytes. Every `Event` carries a
`SourceRef` back to them precisely so the IR can be thrown away and rebuilt.
A session store (see the store design) keeps the native file and treats
`ir.json` as a derived artifact with the reader version stamped on it: if the
stamp does not match, re-read.

The payoff is that **changing the IR costs nothing**. No migration, no
versioned deserializer, no compatibility shims accumulating in `ir.rs`. A field
rename is a recompile. That property is worth more than any amount of schema
versioning machinery, and it is lost the moment something depends on reading a
`SessionIr` written by an older build.

## 2. Semantics belong in the IR, not in the resolver

`src/replay.rs` is the provider-agnostic module: it decides what the model
actually saw. It must not learn any provider's wire vocabulary.

It briefly did, and that is the reason for the rule. The fold matched six
strings out of an untyped `serde_json::Value` — `"turn_aborted"`,
`"thread_rolled_back"` with `data["num_turns"]`, `"last-prompt"` with
`data["leafUuid"]` — two providers' private formats sitting in the one file
supposed to know about none. A third provider with rollback semantics got no
rollback handling, and nothing said so: its control events fell through to
`_ => live.push(id)` and became ordinary conversation content.

The fix was the same move that already worked for compaction. `Compaction` used
to mean "remove these ids", which could not express Codex at all; restating it
as `context` — *the complete post-operation context, a state assignment* — let
one fold serve both providers with no `origin.agent` check anywhere. So the
history-editing semantics are now types:

```rust
Body::Rollback { turns: u32 },   // remove the last `turns` typed turns
Body::Abort {},                  // annotate only; the partial output stayed
```

and, on `SessionIr` rather than on an event — because it is a fact about the
session, not a thing that happened:

```rust
pub live_head: Option<String>,   // head of the live branch; None => no DAG
```

`Body::Control { control_kind, data }` remains, as the catch-all for genuine
chrome and deliberately only that.

A new provider gets correct replay by **emitting the right variant from its
reader**, which is where its wire format is already understood.

`Body::Abort` carries no turn. It briefly did, and on the only provider that
emits aborts the value was identical to `Event::turn` on all 1,821 corpus
occurrences — two fields holding one fact, the second with no reader. A provider
whose abort names some *other* turn brings it back, together with a consumer.
The braces are kept so that adding a field later does not churn every
`Body::Abort { .. }` pattern in the crate.

## 3. No wildcard arm over `Body`

Typed variants alone are not enough. This defeats them:

```rust
_ => live.push(event.id.clone()),
```

A new `Body` variant with history semantics then compiles clean and is silently
treated as ordinary content. So every arm is spelled out — about eleven, several
collapsing together — and in exchange a new variant is a compile error naming
the file that has to decide about it.

The same rule applies to the writers: a `Body` variant no writer handles is a
hole in every conversion, and a wildcard arm is how it stays invisible. One
wildcard survives, in `codex_ir_write::replacement_item`, and it is sound
because it delegates to an exhaustive `payloads()` — verified, not assumed.

**The gate needed the same treatment, and this is the subtle part.** The fold has
to read the directives *before* its visibility filter, because Codex writes both
as `event_msg` and the reader correctly files that as `Ui` — rendering, not
context. Behind the filter, the rollback rule fires on zero of 714 real corpus
rollbacks. Written inline as
`!matches!(body, Body::Rollback { .. } | Body::Abort { .. })`, that exemption is
the one decision point the compiler cannot see: a third directive omitted from
it is silently skipped on exactly the providers the retype was meant to protect.
So it goes through `Body::is_history_directive()`, an exhaustive `match`.

Verified empirically rather than assumed: adding a probe variant produces
`E0004` at five sites, `is_history_directive` among them. Before, it produced
four — and none at the gate.

## 4. Unknown input is data, not an error and not silence

Already the pattern in the readers; keep it.

- An unrecognised record becomes `Body::Unknown { native_type, raw }` and is
  counted in `CaptureReport::unknown`. It is not dropped, and the original
  bytes stay reachable.
- An unrecognised record's visibility is `Visibility::Unclassified`, never
  `Ui`. Defaulting to `Ui` is an assumption that reads as a fact: a future
  release adding a model-context record type would have it classified as
  chrome and silently truncate the conversation. `Unclassified` is never
  replayed and always reported.
- Absence of evidence is not evidence. `ToolOutcome::Unknown` exists because
  100% of Codex tool results were reporting success on no evidence at all.

## 5. Adding a provider to the structured track

The trait already makes this opt-in: `read_session_ir` and `write_session_ir`
default to `Ok(None)`, so a provider joins the high-fidelity track by
overriding them and no provider is ever rewritten to accommodate another.

Override **five** methods, not two. `supports_structured_read` and
`supports_structured_write` are capability probes the pipeline asks *instead of*
calling the method — a capability should not cost a session parse to discover —
so a provider that implements a reader and leaves the probe at `false` is a
provider the pipeline never asks. It will convert, silently, on the flat track.

The fifth is `grade_session_ir`: the grade `write_session_ir` would earn, with
nothing placed on disk. `--dry-run` calls it in the same branch, on the same
three conditions, as the real write — so a dry run predicts the conversion the
user is about to run rather than a flat one they will not. Implement it by
returning the `fidelity` and `losses` of the *same* `render` call your writer
uses and stopping there; deriving the grade a second way is how two sources of
truth for one fact start covering for each other's gaps. Leave it at its default
and your provider will convert correctly and mis-report every dry run of itself.
A writer must also honour the `ContextBudget` it is handed, apply it over
`model_visible` with `ContextBudget::apply`, and fold what it removes into the
losses its grade is derived from; `ContextBudget::UNLIMITED` must produce
byte-identical output. `UNLIMITED` is the common case, not the exception: the
caps are opt-in, so it is what every `resume` that named no budget flag hands
you. That identity is measured rather than asserted — the two structured writers
were run over all 831 readable corpus sessions against a build with the
`ContextBudget::apply` call physically deleted, and all 1,662 renders matched.

**The conformance suite comes with that override, and there is no list to add
yourself to.** `src/conformance.rs` takes
`ProviderRegistry::default_registry()`, keeps the providers whose
`supports_structured_write()` answers `true`, and runs the same battery for
every one of them — as a source and as a write target, so N providers means N²
crossings. Opting in *is* the registration, and it cannot be forgotten, because
forgetting it would mean not being on the track at all.

`tests/conformance_test.rs` is a thin driver: two tiers, the corpus discovery,
the environment sandbox, and the assertions. It contains no per-provider test
body and needs no edit for a third provider.

### What the battery checks

Per session, for every structured provider as the target:

| Check | Fails when |
|---|---|
| Attribution | no structured reader parses the file into a non-empty replay, or a reader stamps `Origin::agent` with something other than its provider's slug — `compare::vendor_of` and every writer's same-agent test key on that string |
| Replay closure | `captured != replayed + superseded + rolled_back + fork + unclassified + chrome + markers`; an id in both `events` and `excluded`; a replayed or excluded id that is not an event; a replayed id belonging to an event the visibility gate dropped; a model-visible event that is neither replayed nor excluded with a reason; `live_head` naming a record that is not in the file |
| Conservation, same agent | model-visible events or capsules are not conserved exactly, an event is invented, the writer claims worse than `ContextComplete`, or it reports any loss at all |
| Prediction, cross agent | the comparator reports anything `unexplained`, or any `carried_foreign` — sealed material that crossed a boundary `Capsule::fits` forbade |
| No overclaim | the grade the writer claims is better than the grade the written file independently supports, re-derived by `compare::Comparison::fidelity` |
| Grade derivation | the claimed `Fidelity` is not the worst grade in the writer's own loss list. Both writers derive it that way; this verifies the property instead of assuming it, because `codex_ir_write::summarise` once accumulated a grade beside its losses and reported one rung better on 126 corpus sessions |
| `Body` coverage | same-agent, a `Body` variant appears in the replays read and not, in the same number, in the replays written back. This is the check that catches a variant added to a reader and forgotten in its writer — named per variant, rather than surfacing as an event-count mismatch |
| Written-session invariants | the file the writer produced does not satisfy the replay closure itself. It is about to be resumed by the real agent |

Every one of those is **two-sided**. A cross-agent crossing whose source
carried sealed material has to both lose it (`target_capsules == 0`, no
`carried_foreign`) *and* report having lost it (a non-empty `predicted`
bucket) — and the second half only activates when the sessions actually
contained sealed material, so it is measured rather than declared. An allowance
that stops being exercised does not stay neutral; it widens until it covers a
regression. `tests/real_world_roundtrip_test.rs::assert_roundtrip_lossless_except`
makes the same argument on the flat side.

**And it prints its tallies whether or not it objects.** Every count above goes
to stderr, per provider and per crossing, including a `Body`-variant matrix
before and after each write. That is not decoration: all five defects in the
table at the top of this document were found by counting something, and none by
a failing assertion. `Report::findings()` is what fails the build;
`Report::print()` is what finds the next one.

### The two things it will tell you to do

Overriding the trait methods is the one-line change. The suite reports the rest
as findings rather than leaving you to discover them:

1. **`compare::vendor_of` needs an arm for the new slug.** Without it, nothing
   can decide whether a capsule may cross into the new provider, and the suite
   says so by name instead of guessing a vendor — guessing would classify every
   capsule as foreign and turn a correct conversion into a verification failure.
2. **The fixtures tier needs at least one session the new reader claims.** Drop
   it anywhere under `tests/fixtures/`; the suite finds it by asking each
   structured reader which files it can parse. Until then the suite reports that
   nothing in the tier was claimed by the new provider — which is the difference
   between "checked" and "quietly checked somebody else's sessions twice".

### Running it

The fixtures tier runs everywhere, in the ordinary `cargo test`. The corpus
tier is `#[ignore]`d because the corpus is machine-local and private, and it
prints a skip banner rather than passing quietly:

```bash
AGS_CODEX_CORPUS="$HOME/.codex/sessions" \
AGS_CLAUDE_CORPUS="$HOME/.claude/projects" \
  cargo test --release --test conformance_test -- --ignored --nocapture
```

Any `AGS_<anything>_CORPUS` variable is picked up, by shape rather than by
name, so a new provider's corpus root needs no edit either.
`AGS_CONFORMANCE_LIMIT` caps the files taken per root.

The corpus is only ever read. Every structured write goes through
`Provider::write_session_ir`, which places the file under the provider's *own*
session root, so the driver points `HOME` at a scratch directory and clears
every provider-specific home override for the duration — and the battery
asserts that each path it wrote landed inside that directory, so a miss in the
sandbox is a loud panic rather than a file in somebody's real session store.
The output of each write is deleted as soon as it has been read back, because
the local corpus is 3.5 GB and every session is written once per target.

### What it measured, and the one thing it found

779 sessions on the reference corpus — 597 Codex rollouts, 182 Claude
transcripts. Same-agent conservation is exact in both directions: 95,275 model
events and 30,143 capsules for Codex, 22,026 and 4,234 for Claude Code, event
for event, capsule for capsule, nothing invented, every session graded
`ContextComplete` with an empty loss list. Cross-agent, nothing is unexplained,
nothing foreign is carried, no grade is overclaimed, and the 352 sealed Codex
compactions become 352 `[converted by casr]` markers. The counts move between
runs because the corpus is being appended to; that is the point of printing them
rather than pinning them.

On its first run it found a real defect and went red, which is the only reason
the defect is known:

> One Claude transcript emitted **10 duplicate `Event::id`s**. Claude Code
> re-appends the records it preserves across a `/compact` immediately before the
> `compact_boundary`, and `claude_code_ir` mints one event per line, so the same
> id was emitted twice. `Event::id` is documented unique within the session and
> `resolve`'s `position` map, `model_visible`'s `by_id` map and `prune_forks`'
> record index all key on it, so which copy survived was arbitrary.

The check was not relaxed for it; the reader was fixed. But the fix is worth
recording, because the first description of the defect — including the one in
this document — was wrong in a way that would have produced a broken fix.

**The re-append is not verbatim.** That word was an assumption, and measuring
all 691 transcripts destroyed it: byte-identity dedupes **0 of the 10**
re-emissions, because Claude stamps a `slug` onto every re-appended copy, and
nine of the ten also carry the *compaction's* `promptId` and the then-current
`cwd`. Comparing whole records, in any form, finds nothing. What works is
equality over the **`Event` the reader built**, minus exactly two fields:
`source`, because a restatement is by definition a different line, and `turn`.

`turn` is the interesting one, and it is a second defect the first description
missed. `Event::turn` is `promptId`, the re-append re-stamps it with the
compaction's own id, and `replay::roll_back` reads `turn` to mean "the last N
typed turns". Had the reader adopted the re-appended value, four historically
distinct turns would have collapsed into one and **a single rollback would have
undone the entire preserved history**. First-occurrence-wins is therefore both
the dedupe rule and the more accurate value — the same choice fixes both bugs.

`ts` is deliberately *not* excluded even though it is identical on all ten
today. If some future Claude release re-stamps the timestamp too, the record
stops being recognised as a restatement and gets a distinct id **and a
counter** — loud — instead of being silently dropped. Strictness that fails
safe is free.

Two mechanisms keep it fixed rather than merely repaired. In the reader a
duplicate id is now unrepresentable: every emission goes through a single
`Sink::emit` door. And `is_restatement` destructures `Event` with no `..`, so
adding an IR field is a build break at the comparison itself — verified by
probe, `E0027` at `claude_code_ir.rs:409`. The uniqueness invariant is enforced
one level lower still, as a `debug_assert_eq!` on `resolve`'s `position` map,
because `resolve` is a stronger chokepoint than the reader trait: the pipeline,
`model_visible` and this very suite are all views over it, it also covers
hand-built and deserialized IR, and no provider can route around it or has to
opt in.

A same-id record whose content genuinely differs is not dropped. It is kept
under `<id>#dup<n>` — a non-numeric suffix, so it cannot collide with an
existing `<uuid>#<slot>` and `record_of` still recovers the record — and
counted. Corpus-wide that path fires zero times, which is the point of counting
it rather than assuming it.

## 6. Grade at the worst point, and carry the grade

`Fidelity` is declared best-first and derives `Ord`, so `worse_of` accumulates
correctly and `max` means "worst". It travels on `StructuredWrite` because only
the writer knows what it had to leave behind; nothing downstream re-derives it.

When a new provider cannot carry something, it adds a rung or reuses one — it
does not quietly succeed. The distinction that matters most is already
encoded: dropping reasoning is `ContextNoReasoning` and survivable, dropping a
`SealedContext` capsule is `HistoryIncomplete` and ranks below even
`TranscriptOnly`, because that blob is not reasoning *about* the conversation,
it *is* the conversation.
