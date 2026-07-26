# The session store

This is the design for task #17 — the "storage" half of the product. It is a
design record, not a tutorial: it says what the store is for, what is
authoritative inside it, and which decisions were deliberate so that a later
change can tell a constraint from an accident.

`casr` converts and forgets. Its output is a file in the target provider's
directory and a line of text on stdout. That is the right shape for a
one-shot converter and the wrong shape for the tool we are building, for one
concrete reason that the corpus can put a number on.

## Why it exists: the second hop

A conversion is lossy in a *direction*. Measured on the corpus:

| hop | reasoning capsules |
|---|---|
| codex → codex | 30,082 → 30,082 |
| codex → claude | 30,082 → 0 (all sealed to `openai`, all correctly refused) |

Now chain it. Convert a Codex session to Claude Code, work there, then convert
back to Codex. The second hop can only carry what the first one left, so the
final Codex session arrives with **0 of the original 30,082 capsules** — even
though the bytes that would have replayed perfectly were sitting in
`~/.codex/sessions` the whole time.

That was a deduction from the two measured hops above. It is now measured
directly, by `conformance::second_hop` (driver:
`tests/conformance_test.rs::the_second_hop_recovers_the_corpus_capsules_the_first_hop_could_not_carry`),
which runs the whole chain through `ConversionPipeline` both ways:

| chain | sessions | capsules in the source | delivered with the store | delivered with `--no-store` |
|---|---|---|---|---|
| codex → claude → codex | 592 | 30,483 | 30,483 | 0 |
| claude → codex → claude | 183 | 4,532 | 4,532 | 0 |

The suite asserts the inequality — consulting the store may never deliver less
sealed material than not consulting it, and a chain whose source carried capsules
may not arrive empty — and prints the numbers. It does not assert them, and the
table above is one run rather than a constant: the corpus is live and grows while
the suite runs against it (two runs an hour apart read 30,622 and 30,483 for the
same chain). Baking a number in would turn a suite that measures the store into
one that measures a particular laptop on a particular afternoon.

One consequence of the ranking is visible in that table and is worth stating
plainly, because it is a trade and not a free win. For a Codex target the origin
wins on capsules *and* on needing no conversion, so the second hop returns the
original Codex session and writes nothing at all. Every capsule survives, and
anything the agent appended on the **Claude** side does not come back with it —
the design has no way to merge two incarnations, and does not claim one. The
choice is stated in the output (`source: codex 01J… (origin; you named
claude-code a3f…, …)`), which is what makes it a choice the user can override
with `--no-store` rather than a surprise.

Nothing in the conversion is wrong. The loss is correct at each hop and
reported at each hop. The defect is that the second hop *asked the wrong
source*. It read the degraded Claude session because that is the session the
user named, when a strictly better source for a Codex target existed.

That is the store's job: **remember that two provider sessions are the same
conversation, so a conversion can choose its source instead of inheriting
it.** Launching and argument passthrough are conveniences. This is the part
that changes results.

## What is authoritative

The same rule as the IR, for the same reason:

> **Native bytes are the source of truth. Everything else in the store is a
> cache and must be reconstructible from them.**

Three things follow, and they are the whole reason the design is small:

- **The stored IR is a cache.** `ir_version` is written into every serialised
  IR and, until now, read by nobody — `IR_VERSION` has been bumped once
  (`agsx-ir/1` → `/2`) with no enforcement anywhere. The store is that
  enforcement point, and the enforcement is *deletion*: on read, a mismatched
  `ir_version` discards the cached IR and re-derives it from origin bytes.
  There is no migration path and there must never be one. This is what keeps
  the IR cheap to change, which is the property the whole design is built to
  protect.
- **The index is a cache.** Answering "which conversation owns Claude session
  `X`?" without a full filesystem scan needs an index, and `rusqlite` is
  already a dependency (the Cursor and Cline providers read SQLite sessions),
  so the index is `index.sqlite` with the schema version in `PRAGMA
  user_version`. A single-file index invites the objection that it is a single
  point of corruption. It is not, because it is rebuildable from the record
  directories: `agsx store fsck --rebuild-index`. Only content is
  authoritative.
- **Losses are records, not caches.** `Fidelity` and `Vec<Loss>` describe an
  event that happened at a point in time to a specific pair of files. They
  cannot be recomputed later — the target session has since been edited by the
  agent — so they are written down and never derived again.

## What it does not do: origin bytes are referenced, not archived

The tempting version of this design copies every origin session into the store
so the record is self-contained. The corpus says no: ~600 Codex rollouts, the
largest 281 MiB. Copying by default turns a converter into a multi-gigabyte
archiver that nobody asked for.

So by default a record **references** its origin: absolute path, content hash,
size, mtime. On use it resolves to one of three answers, all of which it
reports rather than papering over:

- **unchanged** — use it, full fidelity available.
- **grew, stored prefix still matches** — the session log is append-only, so
  this is the normal case for a live session. Use it; the conversation has
  advanced.
- **gone, or diverged** — origin unavailable. Fall back to the best surviving
  incarnation and *say so*, with the grade that fallback costs.

An earlier draft of this document said the store "re-hashes" on use. Measured,
that is wrong: SHA-256 over the largest rollout in the corpus (281.6 MiB) costs
**3.903 s**, and paying it on every lookup would make the store slower than the
conversion it exists to improve. So size and mtime are a cheap negative check
first, and bytes are read only when that check passes — **561 ns** and zero
bytes read on the normal path, a ratio of about 7×10⁶. Truncation is answered
by the length alone.

That buys speed by weakening the claim, so the weakening is made explicit in
the type rather than left implicit: `OriginState::Unchanged { rehashed: false }`
says in the return value that the bytes were not re-read. The residual hazard —
a rewrite that preserves both length and mtime — is documented on
`OriginSnapshot::state`. A cheap check that silently calls itself verification
would be the worse design; one that reports which check it actually ran is not.

`--archive` opts a record into a real byte copy. This is the honest split: a
reference buys availability, and availability is not backup. Defaulting to a
copy would have made the store quietly expensive; defaulting to a reference
makes it quietly less durable, which is why the third case above is a reported
outcome and not a silent downgrade.

## Shape

```
$AGSX_STORE  (default: dirs::data_dir()/agsx, else ~/.agsx)
  store.json                 # store_version; refuse to WRITE a newer store
  index.sqlite               # cache: (provider, session_id) -> record; rebuildable
  records/<uuid>/
    record.json              # lineage: incarnations, fidelity, losses, timestamps
    ir.json                  # cache: keyed by ir_version; deleted on mismatch
    origin/                  # present only under --archive
```

A **record** is one conversation. It holds N **incarnations**, each a
`(provider, provider_session_id)` pair with a role — one `Origin`, and one
`Derived { from, fidelity, losses }` per conversion we performed. The record
id is a fresh UUID minted at first ingest, not derived from content: sessions
are append-only logs that keep growing, so a content-derived id would change
under the conversation it names.

A record holds **exactly one** `Origin`. An earlier draft said "one `Origin`"
in one place and "the latest origin snapshot *per provider*" in another, which
implies several; the first is what is implemented, and `fsck` reports any
record whose origin count is not 1. What is genuinely omitted is *history*:
only the current snapshot of that origin is kept, not its past ones.

`ir.json` caches **the origin's** IR — one file per record, even though a
record has N incarnations. That follows from the invalidation rule, which says
to re-derive from *origin bytes*. The consequence is worth knowing before it
surprises someone: ranking a **derived** candidate always re-parses that
candidate's file, because there is no cache for it.

Measured, that consequence costs nothing worth fixing. A record with one Codex
origin (208 KiB) and two derived Claude incarnations (198 KiB each) — a
conversation converted twice, which is the realistic shape — ranks in **1.63 ms**
once `ir.json` exists, against **550 µs** for a single provider parse (release;
11.6 ms and 4.0 ms unoptimised) and tens of milliseconds for the conversion the
ranking precedes
(`store_test::ranking_a_realistic_candidate_list_is_cheap_next_to_the_conversion`).
So there is no derived-IR cache, and the reason is not only the ~1 ms: a derived
session is the file an agent has been editing, so a cache of it would need an
invalidation rule that the origin's `(size, mtime, prefix hash)` cannot supply.

Also omitted, and additive to add later: any cross-machine sync story.

`ingest_origin` on a key the store already knows as a `Derived` incarnation
must **not** promote it to `Origin`. Doing so would overwrite that
incarnation's recorded `fidelity` and `losses`, and those are a measurement of
an event that has already happened — nothing can retake them. This was a real
defect in the first implementation, now pinned by a test.

## The one interesting function

```rust
/// The best source for converting this conversation into `target`.
fn best_source_for(
    &self,
    record: &Record,
    target: &dyn Provider,
    registry: &ProviderRegistry,
) -> SourceChoice
```

The registry is a parameter and this document's signature did not have one. It
was built inside the function instead, with `ProviderRegistry::default_registry`,
and that is a latent bug rather than a saving: `pipeline.rs` already owns a
registry, reads the chosen candidate through *its* providers, and would have been
ranking through a different instance of the same list. One instance, injected.

Note that it takes the target. There is no global ranking of incarnations,
because sealed material is vendor-bound: a Codex origin beats a Claude
derivative *when the target is Codex*, and the two are worth exactly the same
when the target is Gemini, where neither vendor's capsules can cross. So the
choice runs through `Capsule::fits()` — the machinery that already decides
this at the event level — rather than through a new preference order that
could disagree with it.

`fits()` ranks candidates but does not order the ties — "worth exactly the same
when the target is Gemini" still has to pick one. The order, declared once as
the field order of a `Rank` and walked by the explanation so that the reason
given is the reason that actually decided:

readable → capsules that fit → needs-no-conversion → recorded completeness →
origin-before-derived → recency.

The third rung is the one this document got wrong. It asserted that a Codex
origin beats a Claude derivative for a Codex target, which is true, and left
the symmetric case unstated — where it is **false**. For a Claude target the
Codex origin does *not* win: converting it costs something and the Claude
incarnation is already there. Without a needs-no-conversion rung the ranking
would have been asymmetric in the wrong direction.

Consequences worth naming before they surprise someone:

- The store may read a session the user did not name. That has to be visible
  in the output, not just in the result: "source: codex `01J…` (origin;
  you named claude-code `a3f…`, which would have cost 3 capsules (10536 bytes
  of sealed material))".
- That line is not reachable from the signature above, which is never told what
  the user named — an under-specification in this document, not an error in it.
  The resolution is a deliberate split: `best_source_for` returns **every**
  ranked candidate with its cost, and rendering takes the named session as an
  argument. So the **choice** is independent of what the user asked for and
  only the **explanation** is not, which is the stronger of the two available
  properties.
- `--no-store` must exist and must mean *exactly* today's behaviour — read
  what I named, write where I said, record nothing. It is both the escape
  hatch and the regression test: the byte-identical codex→codex round trip has
  to keep passing through it.

  As wired, the flag is the *absence* of a `Store`, not a second code path:
  `ConversionPipeline::store` is `None`, so the selection step returns on its
  first line and the record write returns on its first line. Pinned four ways —
  no store directory appears
  (`store_pipeline_test::no_store_consults_nothing_and_creates_nothing`), the
  written bytes are identical to a store-backed run's modulo the ids and
  timestamps a writer mints per call
  (`…::no_store_writes_the_same_bytes_as_a_store_with_nothing_better`), the
  codex→codex round trip still grades `ByteIdentical`, writes nothing and leaves
  the source bytes untouched
  (`…::codex_into_itself_stays_byte_identical_through_no_store`), and the
  `resume --json` envelope gains no field
  (`json_contract_test::contract_resume_json_no_store_adds_no_field`).

  The store is therefore **on by default**: the flag's name presupposes it, and
  the payoff above only exists if the store is consulted without being asked.
  What stays conservative is everything around it. Origins are referenced and
  never copied; a `--dry-run` consults an existing store but will not create one,
  because `Store::open` writes `store.json` and a dry run promises to write
  nothing; and **no store failure can fail a conversion** — an unopenable store,
  a record that cannot be ingested, a chosen source that will not parse, a store
  from a newer build that refuses writes, are each reported as a warning and then
  ignored (`store_pipeline_test::a_store_the_pipeline_may_not_write_degrades_to_a_warning`).

## Where this plugs in

- `pipeline.rs` gains a source-selection step ahead of the read, and a record
  write after a successful conversion. The conversion itself does not change.

  One correction from wiring it. The selection sits immediately *after* the flat
  read, not before it, and it cannot sit before it: the store's only external
  identifier is `(provider, provider_session_id)`, and the provider's own id is
  not knowable from a path — `ResolvedSession` carries no id, and `session_id` as
  the user typed it may be a prefix or a filename suffix that several spellings
  share, so keying on it would file one conversation under several ids. The id
  therefore comes from the read. The price is one wasted flat read in the single
  case where the store overrides the choice, which is the case that was going to
  read a second file anyway.
- `launch.rs` can resolve a record id to a target session id, so
  `resume <record-id> --launch cc` works with our identifier instead of
  requiring the user to know the provider's. `launch::session_named_by_record`
  prefers the target's own incarnation — it needs no conversion, so `--launch`
  starts the agent on a session it already has — and falls back to the origin,
  the one incarnation every record has.
- `ir.rs` gains nothing. `IR_VERSION` finally gets a reader.
- The result carries the selection. `ConversionResult::source` is the choice plus
  the session the user named, so the substitution is visible in the value and not
  only in a log: `SourceSelection::line` renders the sentence, and
  `responses::SourceSelectionJson` renders the same information as fields, omitted
  entirely unless the source was substituted.
