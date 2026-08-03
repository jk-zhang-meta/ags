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
which runs the whole chain through `ConversionPipeline` both ways, and does it
twice: once on an untouched intermediate, and once after synthesised turns have
been appended to it.

| chain | arm | sessions | capsules in the source | delivered with the store | delivered with `--no-store` | appended work delivered, store / `--no-store` |
|---|---|---|---|---|---|---|
| codex → claude → codex | nothing appended | 596 | 30,606 | 30,606 | 0 | — |
| codex → claude → codex | work appended (7,786,896 B) | 596 | 30,606 | 0 | 0 | 596 / 596 |
| claude → codex → claude | nothing appended | 179 | 4,526 | 4,526 | 0 | — |
| claude → codex → claude | work appended (317,367 B) | 179 | 4,526 | 0 | 0 | 179 / 179 |

The numbers are printed, never asserted: the corpus is live and grows while the
suite runs against it (four runs read 30,622, 30,483, 30,524 and 30,606 for the
same chain). Baking one in would turn a suite that measures the store into one
that measures a particular laptop on a particular afternoon.

The second row of each pair is the one worth reading twice. With work appended,
the store delivers **zero** capsules where the untouched arm delivers all of
them — and that is correct. It is the trade the corrected ranking exists to make:
the appended turns exist in the Claude session only, nothing can rebuild them,
and the 30,606 capsules the Codex origin holds are content that origin still has.
The store also stops overriding there — it read the session the hop named in all
775 appended chains, against all 775 in the untouched arm — because the session
the user named is now the right one. See "what outranks what" below.

### Why the untouched arm alone was worthless

It was the whole measurement, and it measured the one case where the mechanism
has no value. With nothing appended, the intermediate is a lossy projection of
the origin, so returning the origin is *trivially* correct — and trivially
worthless, because the user already had the origin. The 30,483/30,483/0 result
was real and proved nothing about the case a user is actually in.

That gap hid a real defect for exactly as long as it existed. Ranking capsules
above growth meant a Codex origin beat a Claude derivative for a Codex target *no
matter how much work the user had done in Claude*: the second hop returned the
original Codex session, wrote nothing, and handed back the file the user already
had, while two hours of appended turns went silently missing behind a line that
reads like a win.

So the suite appends before hop two (`conformance::append_turns`), and the
appended turns are appended and not written over: growth is what the store
detects, and a rewrite reads as *divergence* instead. That is also what a real
agent does, since both structured session formats are append-only JSONL logs.

### What the suite asserts now

It used to assert that consulting the store never delivers less sealed material
than not consulting it. That assertion is **false** once the ranking is right,
and the appended rows above are it being false on purpose. The property that is
both true and wanted is narrower and stronger:

> **The store may never deliver an outcome the user would not have got without
> it.** `--no-store` is the baseline for both halves of what is delivered,
> because without a store the user gets exactly the session they named,
> converted.
>
> - **Conversation content is a floor, never a trade.** Anything the `--no-store`
>   arm delivered must be in the store arm's session too. A turn exists in one
>   incarnation only and no conversion can rebuild it, so losing one is
>   unrecoverable and is never justified by anything.
> - **Sealed material is a floor unless it is bought.** The store arm may deliver
>   fewer capsules than the `--no-store` arm only where it delivered content the
>   `--no-store` arm did not.
>
> And where nothing has advanced at all — the untouched arm — neither clause can
> bite, so the old floor still holds there and is still asserted: the store
> delivers at least what `--no-store` does, and a chain whose source carried
> capsules does not arrive empty.

"The appended work arrived" is a substring test on the bytes the chain delivered,
not an event count. An event count is not decidable here: two formats
legitimately split one native line into a different number of events, so a count
that went down could always be explained away as structural — which is how
content loss stays invisible.

One assertion is about the suite rather than about the store: the appended arm
has to have run at all. A second-hop suite that only measures untouched
intermediates is measuring the one case where returning the origin is trivially
correct, and that is the state this suite was in.

Nothing in the conversion is wrong, in either direction. The loss is correct at
each hop and reported at each hop. The defect was that the second hop *asked the
wrong source* — and then, once it started asking, that it asked with the rungs in
the wrong order.

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
  (`ags-ir/1` → `/2`) with no enforcement anywhere. The store is that
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
  directories: `ags store fsck --rebuild-index`. Only content is
  authoritative.
- **Losses are records, not caches.** `Fidelity` and `Vec<Loss>` describe an
  event that happened at a point in time to a specific pair of files. They
  cannot be recomputed later — the target session has since been edited by the
  agent — so they are written down and never derived again.

## Two invocations at once, and which write goes first

Two terminals converting the same session, or a script that fans out, is an
ordinary thing to do — and the first implementation could not survive it. Every
mutation is a read-modify-write across *two* stores: look for an existing record
for `(provider, session_id)`, write `record.json`, insert the index row. Nothing
serialised those three steps, and nothing serialised the index's own lazy
creation either, so two invocations produced any of:

- `database is locked` (`SQLITE_BUSY`) on the **first statement either of them
  ran**. `PRAGMA journal_mode=WAL` converts a rollback-journal database, which
  needs an exclusive lock that SQLite does not wait for: `busy_timeout` does not
  cover it, so on a store whose index does not exist yet — the first two
  concurrent conversions ever — one invocation simply failed.
- `Query returned no rows`, from the same cause one layer up. The index is
  created on first use by reading `PRAGMA user_version` and rebuilding when it
  does not match. Two invocations both read version 0 and both rebuild, so the
  second one's `DROP TABLE` deleted the row the first had just inserted.
- Two records for one conversation, or a silently lost conversion. The lookup
  and the write were far apart, and `record_conversion` re-wrote a `record.json`
  it had read before the other invocation's incarnation was in it — taking that
  conversion's `losses`, which nothing can recompute, with it.

So every mutation now runs inside one `BEGIN IMMEDIATE` … `COMMIT` on
`index.sqlite`: `ingest_origin`, `record_conversion`, `fsck --rebuild-index`,
and the index's own lazy creation, which re-reads `user_version` under the lock
before deciding to rebuild. `IMMEDIATE` takes SQLite's write lock at `BEGIN`
rather than at the first write, so the second invocation waits there and reads
an index the first one has already finished with. WAL is still requested and is
still worth having, but it is now an optimisation the code does not depend on: a
refused conversion is logged, the next uncontended open performs it, and
`IMMEDIATE` plus `busy_timeout` is correct in either journal mode.

Nothing expensive is inside the lock. Hashing an origin (281 MiB in the corpus,
3.9 s) happens before it, and the conversion itself was never near it; the lock
covers reading a record, rewriting it, and pointing the index at it.

**The order inside the lock is `record.json` first, index row second, commit
last** — and that follows from "only content is authoritative" rather than from
taste. A process killed between the two writes can leave exactly one of two
states:

| left behind | repairable? |
|---|---|
| a record with no index row | **yes** — `fsck --rebuild-index` reads it back out of the record directories, which is what that command is for |
| an index row naming a record that was never written | **no** — every lookup of that session resolves to a record the store cannot load, and no rule can invent the content it names |

The ordering makes the repairable state the only reachable one: `record.json` is
renamed into place before the row is inserted, and the row is not durable until
`COMMIT`, so a kill either rolls the row back or leaves a record the index does
not know about yet. **The index may lag the records. It may never lead them.**
That is also what makes a read racing a write safe: a lookup that the index
answers for always finds a `record.json` already on disk, and `write_json`
renames, so a reader sees the whole old file or the whole new one.

### Ordering the calls is only half of it

A kill is one failure; a power cut is the other, and it does not respect the
order the calls were issued in — only the order they were made durable in. That
half was missing. `atomic_write` syncs the staging file's *contents*, but the
rename that publishes them is a change to a **directory**, and a directory
change is not durable until the directory itself is synced. Nothing synced it. A
syscall trace of one conversion showed the asymmetry plainly:

```
fsync  records/<id>/.casr-tmp-…          # the staging file's contents
rename .casr-tmp-…  -> record.json.ags-new-…
rename record.json.ags-new-… -> record.json     # publication: not durable
fsync  index.sqlite-wal                  # SQLite's COMMIT: durable
```

SQLite fsyncs its commit. The store did not fsync the publication that commit is
supposed to happen *after*, so the volatile write went first and the durable one
second — the forbidden row-with-no-record, reached the long way round. So
`write_json` now fsyncs the directory it renamed into, and `commit` fsyncs
`records/` on a record's **first** publication as well, because the
`records/<id>` entry is new then too and an unsynced directory entry takes the
`record.json` inside it with it. ("First" is decided by the absence of
`record.json`, not of the directory: `ingest_origin` creates the directory
before it commits and `--archive` fills it before that.)

The cost is one metadata `fsync` per store write — two or three per conversion —
against a conversion that has already written and fsynced a whole session file.
It is paid on every conversion because the guarantee is. On Windows there is no
directory fsync in `std` (a directory cannot be opened as a `File`), so the sync
is best effort: the Unix path gets the guarantee and Windows rests on NTFS's own
metadata journalling, exactly as it did before. Failing a write over a sync that
platform cannot issue would break "no store failure may fail a conversion" for
nothing.

Reproduce the trace with an `LD_PRELOAD` shim over `fsync`/`rename` — no
in-process test can see a syscall the kernel is asked for but not told to
order — which is why this one is pinned by measurement rather than by a
`#[test]`.

### `fsck --rebuild-index` scans under the lock

A rebuild is `DELETE FROM sessions` followed by the rows **its own scan** saw, so
the scan is half of a read-modify-write and belongs under the same transaction as
the other half. It was not: the scan ran first and the lock was taken after it,
which rebuilt the index from a snapshot another invocation had already
invalidated. Ingest session B while a rebuild is between its scan and its lock,
and the rebuild deletes B's freshly committed row and rewrites only the records
it saw. B's `record.json` is on disk and `find_by_session` misses it, so the next
conversion mints a second record for a conversation that already had one.
Nothing reports it — and `fsck` is the operation this whole argument leans on.

This is the one place the store deliberately holds the lock across a directory
walk. `fsck` is not on the conversion path, and the walk is the same small-file
work `list` already does on every store written by a newer build.

Pinned by five tests in `store_test`: two invocations ingesting one session
converge on one record; two ingesting *different* sessions keep both (the
index-creation race, which has nothing to do with either session); two
conversions of one conversation keep both lineages and both loss lists; a lookup
racing an ingest never sees half a record; and a rebuild cannot erase a
conversion that committed while it scanned. Each of them fails on the
implementation this replaced.

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

An archived origin resolves its **live** file as `Unavailable` — that is *why*
the archive is being read, and it is the only thing that can explain the fallback
to a user — but the archive itself is a byte copy of exactly what was
snapshotted, so it cannot hold anything appended since and every derivative was
made from precisely these bytes. It is therefore `Advance::Unmoved`, not an
unknown inherited from the missing file. Inheriting it made `--archive`
self-defeating: an unknown makes the record unmergeable, an unmergeable record
defers to the session the user named, and the session the user named on the way
back is the derivative — the one incarnation that does *not* hold the sealed
material the archive was kept for. Archive a Codex origin, convert to Claude,
delete the live rollout, convert back, and the archive was skipped in favour of
zero recoverable capsules. Pinned by *an archived origin is still selectable
after its live file is gone*.

## Shape

```
$AGS_STORE  (default: dirs::data_dir()/ags, else ~/.ags)
  store.json                 # store_version; refuse to WRITE a newer store
  index.sqlite               # cache: (provider, session_id) -> record; rebuildable
  records/<uuid>/
    record.json              # lineage: incarnations, fidelity, losses, timestamps
    ir.json                  # cache: keyed by ir_version; deleted on mismatch
    origin/                  # present only under --archive
```

A **record** is one conversation. It holds N **incarnations**, each a
`(provider, provider_session_id)` pair with a role — one
`Origin { snapshot }`, and one `Derived { from, fidelity, losses, snapshot }`
per conversion we performed. The record id is a fresh UUID minted at first
ingest, not derived from content: sessions are append-only logs that keep
growing, so a content-derived id would change under the conversation it names.

Both roles carry a snapshot, and both snapshots answer the same question — has
this file moved since we last looked? — by the same two cheap fields. An
origin's is taken at ingest; a derived incarnation's at `record_conversion`, on
the file this tool has just written. `Derived.snapshot` is an `Option` and
absent means *unknown*, because records written before it existed are never
migrated. See "detecting growth, and what an unknown does".

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
So there is no derived-IR cache, and the reason is the ~1 ms. An earlier draft
gave a second reason — that a derived session is the file an agent has been
editing, so a cache of it would need an invalidation rule nothing could supply —
and that reason has since stopped being true: a derived incarnation now carries
its own `(size, mtime, prefix hash)` snapshot, taken when this tool wrote the
file, because the ranking needs to know whether the agent appended to it. The
invalidation rule exists. The ~1 ms is still not worth a second cache, so the
answer is unchanged and the argument for it is one clause shorter.

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

readable → **has content the other lacks** → capsules that fit →
needs-no-conversion → recorded completeness → origin-before-derived → recency.

The needs-no-conversion rung is the one an earlier draft of this document got
wrong by omission. It asserted that a Codex origin beats a Claude derivative for
a Codex target, which is true, and left the symmetric case unstated — where it is
**false**. For a Claude target the Codex origin does *not* win: converting it
costs something and the Claude incarnation is already there. Without a
needs-no-conversion rung the ranking would have been asymmetric in the wrong
direction.

### What outranks what, and why growth outranks capsules

The second rung is the one this document got wrong outright, by not having it.
With capsules at the top, a Codex origin beat a Claude derivative for a Codex
target *no matter how much work the user had done in Claude*, and the store
answered a request to continue that work by handing back the file it started
from. That is the store discarding the very thing it exists to protect.

The two quantities are not the same kind of thing, and that is the whole
argument. At the moment of derivation a derivative is a lossy **projection** of
its origin, so every capsule it lacks is content the origin *still has* — the
loss is recoverable, by reading the origin, which is exactly what the ranking
does. Turns appended afterwards are content **nothing else has**: no other
incarnation holds them and no conversion can reconstruct them. A recoverable loss
may never outrank an unrecoverable one.

Four cases, and the design has an answer for each:

| origin advanced | derivative advanced | outcome |
|---|---|---|
| no | no | **origin wins.** Neither holds conversation content the other lacks — see the projection argument above — so the capsule rung decides, and it decides for the origin. The original behaviour, and correct. |
| yes | no | **origin wins.** It has both the newer turns and the capsules; there is nothing to trade. |
| no | yes | **the derivative wins**, and the capsule cost is stated. Never silently prefer older-but-richer. |
| yes | yes | **genuine divergence.** This design cannot merge incarnations and does not claim to. |

The last row resolves to *today's behaviour plus a loud warning*, not to a
refusal: the session the user actually named is read, with the specific cost of
each side reported. The reasoning is the load-bearing one, and it is the same
invariant as "no store failure may fail a conversion" applied to a question the
store cannot answer rather than to one it cannot reach — **without the store the
user would have got the named session anyway, so falling back to it means the
store can never make an outcome worse than not having the store.** A hard error
would fail a conversion that `--no-store` performs fine; guessing would silently
drop one side's work.

What that gives up is small and worth naming: the *choice* is no longer strictly
independent of what the user asked for. It is independent wherever the ranking
can decide, and defers to the user only where it cannot — which is the stronger
of the two available properties, since the alternative is deciding by coin-flip.
`SourceChoice::chosen` is still the ranking's own answer, told nothing;
`SourceChoice::resolve(named)` is what the pipeline reads.

### Detecting growth, and what an unknown does

By the same cheap check as an origin reference, because it is the same check:
`(size, mtime)` decides, and the recorded prefix hash is the confirming read only
when it must be. Ranking stays one `stat` per candidate — 561 ns and zero bytes
read — against 3.903 s for a full SHA-256 of the largest rollout.

That needs something the store did not record. `Role::Origin` had a snapshot;
`Role::Derived` had only `from`, `fidelity` and `losses`, so growth in a
derivative was invisible. It now carries its **own** snapshot, taken at
`record_conversion` on the file we have just written — a one-time cost on warm
bytes, paid once per conversion rather than once per ranking. It is the same
`OriginSnapshot`/`OriginState` pair and not a parallel type: both roles want the
same three answers from the same two cheap fields. Only `archived` is
origin-only, and it is an `Option` that a derived incarnation leaves `None`; a
session this tool wrote can be written again, so there is nothing to archive
against its loss.

What differs is not the answer but what is done with it, and the asymmetry is
deliberate. An origin's snapshot *stands in for* bytes the store does not hold,
so a file that diverged from it can no longer be claimed as that origin and the
archived copy — or nothing — takes over. A derived session's snapshot is only a
growth marker on a file this tool wrote; a derivative that diverged is still the
user's own session, still right there, and still the thing they may have spent
two hours in. So it stays readable and its divergence becomes an *unknown*.

Records written before this existed have no derived snapshot, and there is no
migration — the store's rule is that caches are rebuilt and records are never
migrated, and a snapshot is a record: an observation of a file at a moment that
has passed and cannot be retaken. An absent snapshot therefore means **unknown**,
and an unknown must fail safe. "Fail safe" is not obvious here, so it is spelled
out:

- Reading an unknown as *did not advance* resurrects the original defect exactly:
  the origin wins on capsules and the user's work disappears.
- Reading it as *advanced* is worse in the other direction: the derivative would
  then beat an origin the user explicitly named, dropping 30,606 capsules on a
  guess. That is strictly worse than `--no-store`, which is the one thing the
  store may never be.

So an unknown ranks as holding unseen content — it never loses that rung to a
candidate the store *can* vouch for — and it makes the record unmergeable, which
is the same fallback as genuine divergence: read what the user named, and say
why. It self-heals the next time a conversion writes that session.

An unmergeable record is reported through `warnings` rather than through a new
`source_selection` field. By that field's own rule there is nothing to report —
no session was substituted — and growing the substitution contract to cover a
case where nothing was substituted would make it mean two things.

### What the explanation has to say

`SourceChoice::explain(named)` used to frame every choice as strictly better: it
said what taking the user's suggestion *would have cost* and never what not
taking it *did* cost. That reads as a win in exactly the two rows where something
real is being given up — the derivative-wins row and the divergence row — so it
now states the cost in both directions. A line names, in order: the source and
how its snapshot resolved; why the named session was not read, if it was not;
that the record could not be merged, if it could not, with each unmerged
incarnation's state; and what the chosen source **gives up** in sealed material
that another readable incarnation still holds.

Consequences worth naming before they surprise someone:

- The store may read a session the user did not name. That has to be visible
  in the output, not just in the result: "source: codex `01J…` (origin;
  you named claude-code `a3f…`, which would have cost 3 capsules (10536 bytes
  of sealed material) and holds nothing appended since)".
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
