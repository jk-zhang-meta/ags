# Fork baseline

Test state of upstream ags at the fork point, measured **before** any change in
this fork, so later runs can be compared against something real.

- Fork point: `8d94bfbef4364389e4d9e914cddf813930e77429` (upstream 0.2.3, 2026-07-18)
- Toolchain: `rustc 1.99.0-nightly (008fa22ce 2026-07-25)`
- Command: `cargo test --release --no-fail-fast`
- Result: **1159 passed, 13 failed**

`cargo test` defaults to fail-fast and stops after the first failing test
binary, which reports only 28 tests. Use `--no-fail-fast` or the numbers are
meaningless.

## The 13 failures

### 12 are artifacts of running as root

| Count | Tests |
|---|---|
| 9 | `unix_error_paths::write_to_readonly_dir_*`, `atomic_write_integration::*_write_to_readonly_dir_returns_error` |
| 3 | `unix_error_paths::read_unreadable_*_session_file*` |

Each asserts that an operation fails against a `chmod`-restricted path. Root
bypasses the permission bits, so the operation succeeds and the assertion trips.
These are correct tests that cannot pass as root; they say nothing about the code
under test. Run them as an unprivileged user if they need to be exercised.

### 1 was a fixture transport failure

`fixture_agy_simple` — the Antigravity provider returns `title: None` where
`tests/fixtures/expected/agy_simple.json` expects
`"Read data.txt and run echo HELLO_FROM_AGY"`.

The fixture's `conversations/*.db` is a 45-byte stub (literal bytes
`SQLite format 3\0fixture-not-a-re…`), present only so discovery finds the
session. The real content originally lived below a directory named `logs`,
which the synchronized runtime intentionally excludes. The provider derives
the title correctly when that transcript is present; the failure came from the
runtime mirror omitting the fixture. The current test stores the transcript in
a sync-safe fixture path and reconstructs the native Antigravity layout in a
temporary directory.

## Regression rule for this fork

At the fork point, a change was considered clean when it left **1159 passed /
13 failed** unchanged, with the failure set identical to the list above. This
document is a historical snapshot; the current tree uses its full test suite as
the acceptance baseline.
