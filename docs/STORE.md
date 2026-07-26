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
`~/.codex/sessions` the whole time. (Deduced from the two measured hops above,
not separately measured; the conformance suite will measure it once the store
can be asked for a source.)

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
size, mtime. On use, the store re-hashes and gets one of three answers, all of
which it reports rather than papering over:

- **hash matches** — use it, full fidelity available.
- **file grew, prefix matches** — the session log is append-only, so this is
  the normal case for a live session. Use it; the conversation has advanced.
- **gone, or diverged** — origin unavailable. Fall back to the best surviving
  incarnation and *say so*, with the grade that fallback costs.

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

Two deliberate omissions, both additive to reverse if they turn out to be
wanted: only the latest origin snapshot per provider is tracked (no history of
the origin), and there is no cross-machine sync story.

## The one interesting function

```rust
/// The best source for converting this conversation into `target`.
fn best_source_for(&self, record: &Record, target: &dyn Provider) -> Source
```

Note that it takes the target. There is no global ranking of incarnations,
because sealed material is vendor-bound: a Codex origin beats a Claude
derivative *when the target is Codex*, and the two are worth exactly the same
when the target is Gemini, where neither vendor's capsules can cross. So the
choice runs through `Capsule::fits()` — the machinery that already decides
this at the event level — rather than through a new preference order that
could disagree with it.

Consequences worth naming before they surprise someone:

- The store may read a session the user did not name. That has to be visible
  in the output, not just in the result: "source: codex `01J…` (origin;
  you named claude-code `a3f…`, which would have cost 30,082 capsules)".
- `--no-store` must exist and must mean *exactly* today's behaviour — read
  what I named, write where I said, record nothing. It is both the escape
  hatch and the regression test: the byte-identical codex→codex round trip has
  to keep passing through it.

## Where this plugs in

- `pipeline.rs` gains a source-selection step ahead of the read, and a record
  write after a successful conversion. The conversion itself does not change.
- `launch.rs` can resolve a record id to a target session id, so
  `resume <record-id> --launch cc` works with our identifier instead of
  requiring the user to know the provider's.
- `ir.rs` gains nothing. `IR_VERSION` finally gets a reader.
