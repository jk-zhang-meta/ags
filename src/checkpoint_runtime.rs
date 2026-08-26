//! Embedded AGS checkpoint runtime.
//!
//! The Bash program owns checkpoint encryption, synchronization, and restore
//! transactions. Rust only selects a supported Bash and exposes the current
//! `ags` executable for cross-provider conversion.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail};

const AGS_RUNTIME: &str = include_str!("../plugins/ags/scripts/ags");
const AGS_SKILL: &str = include_str!("../plugins/ags/skills/ags/SKILL.md");
const AGS_OPENAI_AGENT: &str = include_str!("../plugins/ags/skills/ags/agents/openai.yaml");

/// Run the embedded checkpoint runtime with its arguments unchanged.
pub fn run(args: &[OsString]) -> Result<ExitStatus> {
    let (_script, mut command) = runtime_command(args)?;
    command
        .status()
        .context("failed to start the AGS checkpoint runtime")
}

fn runtime_command(args: &[OsString]) -> Result<(tempfile::NamedTempFile, Command)> {
    let (bash, homebrew_prefix) = runtime_bash()?;
    let current_exe = std::env::current_exe().context("cannot locate the ags executable")?;
    let mut script = tempfile::Builder::new()
        .prefix("ags-checkpoint-")
        .suffix(".sh")
        .tempfile()
        .context("cannot create the AGS checkpoint runtime file")?;
    script
        .write_all(AGS_RUNTIME.as_bytes())
        .context("cannot write the AGS checkpoint runtime file")?;
    script
        .flush()
        .context("cannot flush the AGS checkpoint runtime file")?;

    let mut command = Command::new(bash);
    command
        .arg("-c")
        .arg("script=$1; shift; source \"$script\"")
        .arg("ags")
        .arg(script.path())
        .args(args)
        .env("AGS_CONVERTER_BINARY", current_exe)
        .env("AGS_CONVERTER_VERSION", env!("CARGO_PKG_VERSION"));
    if let Some(prefix) = homebrew_prefix {
        command.env("AGENT_SESSION_HOMEBREW_PREFIX", prefix);
    }

    Ok((script, command))
}

/// Return an installer asset carried by the verified ags binary.
pub fn asset(name: &str) -> Option<&'static str> {
    match name {
        "skill" => Some(AGS_SKILL),
        "openai-agent" => Some(AGS_OPENAI_AGENT),
        _ => None,
    }
}

fn runtime_bash() -> Result<(PathBuf, Option<PathBuf>)> {
    match std::env::consts::OS {
        "linux" => Ok((
            which::which("bash").context("AGS checkpoints require Bash 4 or newer")?,
            None,
        )),
        "macos" => {
            let prefix = homebrew_prefix()?;
            let bash = prefix.join("bin/bash");
            if !bash.is_file() {
                bail!(
                    "Homebrew Bash is unavailable at {}; run `brew install bash`",
                    bash.display()
                );
            }
            Ok((bash, Some(prefix)))
        }
        os => bail!("AGS checkpoints are unsupported on {os}"),
    }
}

fn homebrew_prefix() -> Result<PathBuf> {
    // Do not trust an ambient prefix or PATH here: hooks may run with inherited
    // environment variables, and this path is used to select an executable.
    for prefix in ["/opt/homebrew", "/usr/local"] {
        let prefix = PathBuf::from(prefix);
        if prefix.join("bin/bash").is_file() && prefix.join("opt/util-linux/bin/column").is_file() {
            return Ok(prefix);
        }
    }

    bail!("AGS checkpoints require Homebrew Bash and util-linux in /opt/homebrew or /usr/local")
}
