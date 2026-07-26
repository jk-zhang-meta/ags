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

What is still missing is a **conformance suite** — one battery every structured
provider must pass, so that adding a provider is one line in a list and the
whole battery comes with it:

- read → `resolve()` → every returned id is a `Model` event, no duplicates, and
  `resolved + superseded + rolled_back + abandoned_fork == captured` closes
  exactly (it does today for both providers);
- read → write → read, same agent: model-visible content identical, capsule
  count identical per carrier;
- cross-agent: exactly the capsules `Capsule::fits()` predicted are missing and
  nothing else;
- every `Body` variant the reader can emit is handled by the writer;
- run against the real corpus, and print the counts rather than only asserting
  them — a number on screen is what caught all five defects above.

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
