//! Test-only probes for whether this process is actually subject to file
//! permissions — and an announcement when it is not.
//!
//! A test that constructs a `0o000` file or a `0o555` directory and asserts
//! that the code under test is denied is only meaningful when the running
//! process is denied too. Root holds `CAP_DAC_OVERRIDE`, so under a container,
//! a CI image that runs as root, or a local root shell, the mode is advisory:
//! the read succeeds, the write succeeds, and the denial under test cannot be
//! constructed at all.
//!
//! Probing for that is correct and both callers already did it. What they did
//! not do is say so. A bare `return` makes the harness print `ok` for a test
//! that ran no assertion, so a suite can report full green while eleven of its
//! permission tests asserted nothing — which is what happened here, on every
//! root run. `list_truthfulness_test.rs` had the announcement and these did
//! not; this module is that pattern, in one place, for the two binaries that
//! had a copy of the probe each.
//!
//! The capability is probed rather than inferred from the uid, because
//! "running as root" and "root can bypass *this* filesystem's permissions"
//! are different statements and only the second one makes a test vacuous.

// Each including binary uses a different subset of these probes, so an unused
// one is expected rather than dead. The crate builds with `-D warnings`.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// Emit `message` on every channel that can still carry it out of a *passing*
/// test. Neither write alone is enough, and neither is complete.
///
/// libtest captures each test's output and discards it unless that test fails
/// or the run passes `--nocapture` / `--show-output`. So `eprintln!` alone is
/// invisible under the plain `cargo test` everyone runs, which would leave this
/// module solving the problem only for people who already suspected it.
///
/// Capture works by swapping Rust's `std::io::stderr` sink rather than by
/// redirecting file descriptor 2, so opening the process's real stderr by path
/// steps around it. Measured, on this suite, as root:
///
/// | how the run is invoked          | `/dev/stderr` | `eprintln!`         |
/// |---------------------------------|---------------|---------------------|
/// | terminal                        | all 11        | dropped             |
/// | `cargo test \| tee`, CI logs    | all 11        | dropped             |
/// | `cargo test > log 2>&1`         | **unreliable**| dropped             |
/// | `--nocapture` / `--show-output` | all 11        | all 11 (duplicated) |
///
/// The `> log` row is the honest limit and it is not a fixed fraction: a single
/// test binary kept 3 of its 11, the whole suite kept none. Cargo writes to
/// fd 2 at its own file offset while these writes go to end-of-file, so cargo's
/// next write lands on top of whatever is there, and the more cargo writes the
/// less survives. **Pipe the run** (`cargo test 2>&1 | tee log`) if you want
/// these out of a file; that is measured at all 11 for the full suite, and it
/// is what a terminal and a CI log already do.
///
/// `eprintln!` is kept because it is the only channel that reaches the JSON
/// test report CI produces with `--show-output`.
fn announce(message: &str) {
    use std::io::Write;

    if let Ok(mut real_stderr) = fs::OpenOptions::new().append(true).open("/dev/stderr") {
        let _ = writeln!(real_stderr, "{message}");
    }
    eprintln!("{message}");
}

/// Whether `path` actually rejects a write by this process.
///
/// Returns `false` — after saying why, and where — when the directory accepts
/// a write despite its mode. Callers treat `false` as "skip the assertion".
#[track_caller]
pub fn directory_rejects_writes(path: &Path) -> bool {
    let probe = path.join(".casr-permission-probe");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            announce(&format!(
                "skipping at {}: this process writes into {} despite its read-only mode \
                 (root with CAP_DAC_OVERRIDE), so the denial under test cannot be \
                 constructed and nothing below was asserted",
                std::panic::Location::caller(),
                path.display(),
            ));
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => true,
        Err(error) => {
            // Not the capability case: the fixture this test needs is not in
            // place. Skipping silently here would hide a broken setup behind
            // the same green `ok` as the capability skip.
            announce(&format!(
                "skipping at {}: probing {} failed with `{error}` rather than a permission \
                 denial, so this test's setup is not in place and nothing below was asserted",
                std::panic::Location::caller(),
                path.display(),
            ));
            false
        }
    }
}

/// Whether `path` actually rejects a read by this process.
///
/// The read-side counterpart of [`directory_rejects_writes`], with the same
/// contract: `false` means the assertion below it is vacuous and was skipped,
/// and the reason is on stderr.
#[track_caller]
pub fn file_rejects_reads(path: &Path) -> bool {
    match fs::File::open(path) {
        Ok(_) => {
            announce(&format!(
                "skipping at {}: this process opens {} despite its 0o000 mode \
                 (root with CAP_DAC_OVERRIDE), so the denial under test cannot be \
                 constructed and nothing below was asserted",
                std::panic::Location::caller(),
                path.display(),
            ));
            false
        }
        Err(_) => true,
    }
}
