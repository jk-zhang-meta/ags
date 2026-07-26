# Attribution and licensing

## This is a fork

`agsx-convert` is a fork of **casr (Cross Agent Session Resumer)** by Jeffrey Emanuel.

| | |
|---|---|
| Upstream | <https://github.com/Dicklesworthstone/cross_agent_session_resumer> |
| Fork point | `8d94bfbef4364389e4d9e914cddf813930e77429` (2026-07-18) |
| Upstream version at fork point | 0.2.3 |
| Upstream copyright | Copyright (c) 2026 Jeffrey Emanuel |

The upstream remote is retained as `upstream` so changes can be tracked and,
where useful, contributed back.

## License is unchanged, deliberately

`LICENSE` is upstream's **MIT License with an OpenAI/Anthropic Rider**, kept
byte-for-byte (SHA-256 `32a82e0a5754e72e51fae44b65a936c831c07376f21c90f5fb9e76897fcc3509`).

The rider requires that

> any distribution of the Software or any Derivative Works must include this
> rider provision unmodified.

This fork is a Derivative Work, so the rider is carried forward as written.
Anyone redistributing this fork, or anything derived from it, inherits the same
obligation.

Two consequences worth stating plainly:

1. **This project is not OSI-approved open source.** The rider withholds rights
   from named parties and restricts machine-learning use, which is incompatible
   with clauses 5 and 6 of the Open Source Definition. Do not label it "open
   source" without that qualification.
2. **The license cannot be changed unilaterally.** Relicensing this fork — to
   plain MIT or anything else — requires written permission from Jeffrey
   Emanuel.

## Why `ags` is kept in a separate repository

[`ags`](../ags) stays under a plain MIT license and does **not** vendor,
statically link, or otherwise incorporate any code from this fork. It invokes
`agsx-convert` as an external executable across a process boundary, in the same
way it previously invoked `transession`.

That boundary is the point: it keeps the rider contained in this repository
instead of propagating into `ags` and, from there, into everyone who uses `ags`.
Anything that would merge the two codebases into one distributable artifact
should be treated as a licensing decision, not a refactor.

## What this fork changes

Upstream casr normalizes every provider's session into a flat text model
(`CanonicalMessage.content: String`). That is sufficient for conversational
handoff and is retained here for the providers that only need it.

This fork adds a second, structured track for the providers where fidelity
matters, and reports honestly on which track was used:

- a typed event IR alongside the flat model, rather than replacing it, so the
  providers that do not need it keep working unchanged;
- verbatim carriage of provider-bound reasoning material — Anthropic
  `thinking.signature`, OpenAI `reasoning.encrypted_content` — which upstream
  discards, so that same-provider handoffs stop losing reasoning context;
- modelling of context compaction, which appears in roughly three quarters of
  real Codex rollouts and, if ignored, causes pre-compaction history to be
  replayed to the target agent;
- preservation of the Codex freeform/`custom_tool_call` protocol rather than
  rewriting it into `function_call`;
- a structural read-back comparator, replacing one that compared only message
  count, role bucket, and flattened text and therefore could not detect any loss
  the flat model had already committed;
- explicit fidelity classes on every conversion, so a lossy handoff is labelled
  as one instead of being presented as a restore.

Upstream's provider discovery, native-format parsing, atomic write and rollback
machinery, CLI ergonomics, and test suite are the reason this fork exists rather
than a from-scratch implementation. That work is Jeffrey Emanuel's.
