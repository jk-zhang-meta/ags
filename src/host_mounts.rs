//! Host-side workspace planning, fail-closed sensitive path guard, single-file
//! read-only snapshot staging, and dynamic VirtioFS / bwrap external mount
//! registry (`MountSession`).
//!
//! Security invariants (verified by adversarial architecture review):
//! 1. **Zero network/proxy mount authority**: `CageShim` NEVER holds a reference
//!    to `MountSession` and NEVER probes the host filesystem. Grants are created
//!    exclusively by trusted host-side user actions: startup CLI flags
//!    (`--mount`, `--ro-mount`, `--add-dir`), host-side bracketed paste / Finder
//!    drop interception, or the host-only `Ctrl+]` interactive attach prompt.
//! 2. **Object-identity denylist (`st_dev`, `st_ino`)**: Protected roots and
//!    home-relative sensitive paths are checked by both normalized path ancestry
//!    and Unix `(st_dev, st_ino)` across every ancestor, defeating macOS APFS
//!    case-insensitivity, Unicode NFC/NFD, `/System/Volumes/Data` firmlinks, and
//!    symlink tricks.
//! 3. **Single-file RO snapshot isolation**: Attaching an external single file
//!    (e.g., `~/Desktop/bug.png` or `~/Code/other/auth.rs`) opens it with
//!    `O_NOFOLLOW`, validates `fstat` (regular file, `nlink == 1`, bounded size),
//!    and atomically copies bytes into a session-private read-only staging
//!    directory exported at `/mnt/ext/_files/` — disclosing ZERO sibling files
//!    and requiring ZERO VirtioFS share-table replacement.
//! 4. **Per-workspace session isolation**: Each `WorkspacePlan` carries a
//!    deterministic `workspace_id` derived from the canonical host workspace root
//!    so concurrent projects both mapped to `/workspace` never collide on trust
//!    decisions or `--auto-rebind` transcripts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::berth::BerthError;

/// Maximum size of a single external file snapshotted into `/mnt/ext/_files/`.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Guest mount point for the active project root.
pub const GUEST_WORKSPACE_ROOT: &str = "/workspace";

/// Guest mount point for explicitly authorized external folders & staged files.
pub const GUEST_EXT_ROOT: &str = "/mnt/ext";

/// Subdirectory inside `/mnt/ext` backed by the session-private RO staging dir.
pub const GUEST_FILES_SUBDIR: &str = "_files";

/// Maximum control frame size (bytes) on the private `bh` <-> `berth-vz` socketpair.
pub const MAX_MOUNT_CTL_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountAccess {
    Ro,
    Rw,
}

impl MountAccess {
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Ro)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    #[cfg(not(unix))]
    pub fn from_metadata(_meta: &fs::Metadata) -> Self {
        Self { dev: 0, ino: 0 }
    }

    /// Returns Unix `(st_dev, st_ino)` on Unix, or a deterministic 128-bit SHA-256
    /// path-identity hash on non-Unix platforms so distinct grants never alias `(0, 0)`.
    pub fn from_path_and_metadata(path: &Path, meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            let _ = path;
            Self::from_metadata(meta)
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            let mut h = Sha256::new();
            h.update(b"berth-path-id-v1:");
            h.update(path.as_os_str().as_encoded_bytes());
            let d = h.finalize();
            let dev = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
            let ino = u64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
            Self { dev, ino }
        }
    }
}

/// Authoritative resolved workspace mapping shared across launch, resume, trust,
/// rebind, Micro-VM, and Linux `bwrap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePlan {
    /// Canonical host directory mounted at `/workspace`.
    pub host_root: PathBuf,
    /// Verified `FileIdentity` (`st_dev`, `st_ino`) of `host_root` captured at authorization time.
    pub host_root_id: FileIdentity,
    /// Canonical host `cwd` where the user invoked `bh`.
    pub host_cwd: PathBuf,
    /// Relative path from `host_root` to `host_cwd` (empty when equal).
    pub relative_cwd: PathBuf,
    /// Exact guest working directory (`/workspace` or `/workspace/<relative_cwd>`).
    pub guest_cwd: PathBuf,
    /// Deterministic 12-hex ID of `host_root` used to isolate per-project trust
    /// and transcripts when multiple repos map to `/workspace`.
    pub workspace_id: String,
    /// True if `.git` in `host_root` is a pointer file (linked worktree / submodule /
    /// external gitdir) whose target was NOT exported outside the workspace.
    pub has_external_gitdir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveDirGrant {
    pub alias: String,
    pub host_path: PathBuf,
    pub access: MountAccess,
    pub generation: u64,
    #[serde(default)]
    pub identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshotGrant {
    pub alias: String,
    pub source_identity: FileIdentity,
    pub staged_path: PathBuf,
    pub guest_path: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantOutcome {
    pub guest_path: String,
    pub access: MountAccess,
    pub is_file_snapshot: bool,
    pub needs_vz_update: bool,
    pub base_generation: u64,
    pub next_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteRewriteOutcome {
    /// Text contained no host path references; forward unchanged.
    Unchanged,
    /// One or more host paths were authorized and replaced with neutral guest paths.
    Rewritten {
        text: String,
        notices: Vec<String>,
        needs_vz_update: bool,
        base_generation: u64,
        next_generation: u64,
    },
    /// At least one referenced host path violated the security policy; do NOT forward raw path.
    Blocked { reason: String },
}

/// Sensitive path guard enforcing both path-ancestry and Unix `(st_dev, st_ino)` checks.
#[derive(Debug, Clone)]
pub struct SensitivePathGuard {
    real_home: PathBuf,
    home_variants: Vec<PathBuf>,
    /// Exact directories that MUST NOT be granted directly, while selected safe
    /// descendants ARE allowed (e.g., `$HOME/Library/CloudStorage` and its immediate
    /// provider folders like `OneDrive-Personal`, while `$HOME/Library/CloudStorage/OneDrive-Personal/Work` is allowed).
    exact_only_denied_paths: Vec<PathBuf>,
    exact_only_denied_ids: BTreeSet<FileIdentity>,
    /// Directories whose entire subtrees are unconditionally denied (both the dir
    /// itself and any descendant, AND any ancestor directory containing them!).
    subtree_denied_paths: Vec<PathBuf>,
    subtree_denied_ids: BTreeSet<FileIdentity>,
}

impl SensitivePathGuard {
    pub fn new(
        real_home: &Path,
        state_dir: &Path,
        config_dir: &Path,
        extra_protected: &[&Path],
    ) -> Self {
        let canon_home = best_effort_canonical(real_home);
        let mut home_variants = vec![canon_home.clone()];
        if let Some(stripped) = strip_macos_data_volume(&canon_home) {
            if !home_variants.contains(&stripped) {
                home_variants.push(stripped);
            }
        }
        let raw_home = real_home.to_path_buf();
        if !home_variants.contains(&raw_home) {
            home_variants.push(raw_home);
        }

        let mut exact_only_denied_paths: Vec<PathBuf> = Vec::new();
        let mut exact_only_denied_ids: BTreeSet<FileIdentity> = BTreeSet::new();
        let mut subtree_denied_paths: Vec<PathBuf> = Vec::new();
        let mut subtree_denied_ids: BTreeSet<FileIdentity> = BTreeSet::new();

        let mut add_exact = |p: PathBuf| {
            if let Ok(meta) = fs::symlink_metadata(&p) {
                exact_only_denied_ids.insert(FileIdentity::from_metadata(&meta));
            }
            if let Ok(c) = fs::canonicalize(&p) {
                if let Ok(meta) = fs::metadata(&c) {
                    exact_only_denied_ids.insert(FileIdentity::from_metadata(&meta));
                }
                if !exact_only_denied_paths.contains(&c) {
                    exact_only_denied_paths.push(c.clone());
                }
                if let Some(s) = strip_macos_data_volume(&c) {
                    if !exact_only_denied_paths.contains(&s) {
                        exact_only_denied_paths.push(s);
                    }
                }
            }
            if !exact_only_denied_paths.contains(&p) {
                exact_only_denied_paths.push(p);
            }
        };

        let mut add_subtree = |p: PathBuf| {
            if let Ok(meta) = fs::symlink_metadata(&p) {
                subtree_denied_ids.insert(FileIdentity::from_metadata(&meta));
            }
            if let Ok(c) = fs::canonicalize(&p) {
                if let Ok(meta) = fs::metadata(&c) {
                    subtree_denied_ids.insert(FileIdentity::from_metadata(&meta));
                }
                if !subtree_denied_paths.contains(&c) {
                    subtree_denied_paths.push(c.clone());
                }
                if let Some(s) = strip_macos_data_volume(&c) {
                    if !subtree_denied_paths.contains(&s) {
                        subtree_denied_paths.push(s);
                    }
                }
            }
            if !subtree_denied_paths.contains(&p) {
                subtree_denied_paths.push(p);
            }
        };

        // Whole-filesystem / container roots (cannot be granted themselves, though safe descendants may be).
        for root_str in ["/", "/Users", "/home", "/Volumes", "/System/Volumes/Data", "/System/Volumes/Data/Users"] {
            add_exact(PathBuf::from(root_str));
        }
        add_exact(canon_home.clone());
        let cloud_root = canon_home.join("Library").join("CloudStorage");
        add_exact(cloud_root.clone());
        // Deny direct CloudStorage provider roots (e.g. `~/Library/CloudStorage/OneDrive-Personal`)
        // while allowing specific project subfolders underneath them.
        if let Ok(entries) = fs::read_dir(&cloud_root) {
            for ent in entries.flatten() {
                add_exact(ent.path());
            }
        }
        add_exact(canon_home.join("Library"));

        // OS system roots (entire subtrees denied).
        for sys_dir in [
            "/System",
            "/Library",
            "/etc",
            "/private/etc",
            "/private/var",
            "/var",
            "/proc",
            "/sys",
            "/dev",
        ] {
            // Note: on macOS `/var/folders` / `/private/var/folders` holds user tempdirs in tests;
            // keep `/private/etc`, `/System`, `/Library` strictly denied and protect `/private/var/db`,
            // `/private/var/root`, `/private/var/run`, `/private/var/log`, `/private/var/mail` specifically.
            if sys_dir == "/var" || sys_dir == "/private/var" {
                add_exact(PathBuf::from(sys_dir));
                for sub in ["db", "root", "run", "log", "mail", "spool", "backups", "lib", "audit"] {
                    add_subtree(Path::new(sys_dir).join(sub));
                }
            } else {
                add_subtree(PathBuf::from(sys_dir));
            }
        }

        // Berth own control/state/config/staging directories (entire subtrees denied).
        add_subtree(best_effort_canonical(state_dir));
        add_subtree(best_effort_canonical(config_dir));
        for p in extra_protected {
            add_subtree(best_effort_canonical(p));
        }

        // Home-relative sensitive credential / personal data subtrees.
        for rel in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".kube",
            ".docker",
            ".claude",
            ".codex",
            ".gemini",
            ".config/berth",
            ".config/gh",
            ".config/gcloud",
            ".local/state/berth",
            ".local/state/ags",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".zsh_history",
            ".bash_history",
            ".git-credentials",
            ".gitconfig",
            "Library/Keychains",
            "Library/Cookies",
            "Library/Preferences",
            "Library/Application Support",
            "Library/Mail",
            "Library/Messages",
            "Library/Safari",
            "Library/Group Containers",
            "Library/Accounts",
        ] {
            add_subtree(canon_home.join(rel));
        }

        Self {
            real_home: canon_home,
            home_variants,
            exact_only_denied_paths,
            exact_only_denied_ids,
            subtree_denied_paths,
            subtree_denied_ids,
        }
    }

    pub fn real_home(&self) -> &Path {
        &self.real_home
    }

    pub fn home_variants(&self) -> &[PathBuf] {
        &self.home_variants
    }

    /// Validates raw input path syntax (rejects NUL/control, `..`, `~otheruser`, URIs)
    /// and expands current-user `~/...` into an absolute host `PathBuf`.
    pub fn parse_and_expand_host_input(&self, raw: &str) -> Result<PathBuf, BerthError> {
        self.parse_and_expand_host_input_opts(raw, true)
    }

    /// Same as `parse_and_expand_host_input`, with explicit control over leading/trailing
    /// whitespace trimming (`trim_ws = false` for explicit CLI path arguments so a directory
    /// legitimately ending with a space NEVER silently grants its trimmed sibling!).
    pub fn parse_and_expand_host_input_opts(
        &self,
        raw: &str,
        trim_ws: bool,
    ) -> Result<PathBuf, BerthError> {
        let trimmed = if trim_ws { raw.trim() } else { raw };
        if trimmed.is_empty() {
            return Err(BerthError::InvalidArgument("host path must not be empty".into()));
        }
        if trimmed.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(BerthError::InvalidArgument(
                "host path contains control characters".into(),
            ));
        }
        let lc = trimmed.to_ascii_lowercase();
        if lc.starts_with("file://") || lc.contains("://") {
            return Err(BerthError::InvalidArgument(
                "URI paths are not accepted; provide a local filesystem path".into(),
            ));
        }
        let expanded = if trimmed == "~" {
            self.real_home.clone()
        } else if let Some(rest) = trimmed.strip_prefix("~/") {
            self.real_home.join(rest)
        } else if trimmed.starts_with('~') {
            return Err(BerthError::InvalidArgument(
                "other-user tilde expansion (`~user`) is forbidden".into(),
            ));
        } else {
            let p = PathBuf::from(trimmed);
            if !p.is_absolute() {
                return Err(BerthError::InvalidArgument(format!(
                    "host path `{trimmed}` must be absolute or start with `~/`"
                )));
            }
            p
        };

        for comp in expanded.components() {
            if matches!(comp, Component::ParentDir) {
                return Err(BerthError::InvalidArgument(format!(
                    "parent traversal `..` is forbidden in host path `{trimmed}`"
                )));
            }
        }
        Ok(expanded)
    }

    /// Validates a host path against all protected roots, `(st_dev, st_ino)` identities,
    /// ancestor containment, and (when `check_sensitive_basename` is true) sensitive filenames.
    /// Returns the canonicalized path and its verified `FileIdentity`.
    pub fn validate_existing_path(
        &self,
        candidate: &Path,
        expect_dir: Option<bool>,
        check_sensitive_basename: bool,
    ) -> Result<(PathBuf, FileIdentity, fs::Metadata), BerthError> {
        // First reject raw symlinked or non-existent components along the lexical path
        // only when an ancestor hits a protected directory BEFORE canonicalization too.
        self.check_lexical_ancestors(candidate)?;

        let canon = fs::canonicalize(candidate).map_err(|e| {
            BerthError::InvalidArgument(format!(
                "host path `{}` does not exist or cannot be accessed: {e}",
                candidate.display()
            ))
        })?;
        let meta = fs::metadata(&canon).map_err(|e| {
            BerthError::InvalidArgument(format!(
                "cannot stat host path `{}`: {e}",
                canon.display()
            ))
        })?;
        let id = FileIdentity::from_path_and_metadata(&canon, &meta);

        // Reject FIFOs, sockets, devices (only regular files and directories are allowed).
        if !meta.is_dir() && !meta.is_file() {
            return Err(BerthError::InvalidArgument(format!(
                "refusing special filesystem object `{}` (only regular files and directories allowed)",
                canon.display()
            )));
        }
        if let Some(want_dir) = expect_dir {
            if want_dir && !meta.is_dir() {
                return Err(BerthError::InvalidArgument(format!(
                    "expected directory, got file `{}`",
                    canon.display()
                )));
            }
            if !want_dir && !meta.is_file() {
                return Err(BerthError::InvalidArgument(format!(
                    "expected regular file, got directory `{}`",
                    canon.display()
                )));
            }
        }

        // Check exact-only denied roots (`/` , `$HOME`, `/Users`, `~/Library/CloudStorage`, etc.)
        // AND dynamic CloudStorage provider roots (`~/Library/CloudStorage/<provider>`) + WSL
        // Windows profile roots (`/mnt/<drive>/Users` and `/mnt/<drive>/Users/<profile>`).
        #[cfg(unix)]
        let exact_id_hit = self.exact_only_denied_ids.contains(&id);
        #[cfg(not(unix))]
        let exact_id_hit = false;
        if exact_id_hit
            || self.matches_exact_denied_path(&canon)
            || self.is_dynamic_exact_denied_root(&canon)
        {
            return Err(BerthError::InvalidArgument(format!(
                "refusing protected root directory `{}` (select a specific project subfolder instead)",
                canon.display()
            )));
        }

        // Walk EVERY ancestor of `canon` for `(st_dev, st_ino)` checks, and walk the
        // `/System/Volumes/Data`-stripped spelling for lexical path checks so safe APFS
        // paths whose canonical form retains `/System/Volumes/Data` don't hit `/System`.
        #[cfg(unix)]
        for anc in canon.ancestors() {
            if let Ok(ameta) = fs::metadata(anc) {
                let aid = FileIdentity::from_metadata(&ameta);
                if self.subtree_denied_ids.contains(&aid) {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing protected path `{}` (blocked ancestor `{}`)",
                        canon.display(),
                        anc.display()
                    )));
                }
            }
        }
        let canon_logical = strip_macos_data_volume(&canon).unwrap_or_else(|| canon.clone());
        for anc in canon_logical.ancestors() {
            if self.matches_subtree_denied_path(anc) || self.is_wsl_windows_sensitive_path(anc) {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing protected path `{}` (blocked path `{}`)",
                    canon.display(),
                    anc.display()
                )));
            }
        }

        // If granting a directory, ensure it does NOT contain any static OR dynamic protected
        // subtree (e.g., `/Users/alice`, `~/Library`, WSL `/mnt/<drive>/Users/<me>/.config`, etc.).
        if meta.is_dir() {
            for prot in &self.subtree_denied_paths {
                if path_starts_with_os(prot, &canon)
                    || path_starts_with_os(prot, &canon_logical)
                {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing directory `{}` because it contains protected path `{}`",
                        canon.display(),
                        prot.display()
                    )));
                }
            }
            if self.is_dynamic_protected_container(&canon_logical) {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing directory `{}` because it contains dynamically protected user credentials/state",
                    canon.display()
                )));
            }
            // Reject another user's home directory (`/Users/<other>`, `/home/<other>`, or WSL `/mnt/<drive>/Users/<other>`).
            if self.is_other_users_home(&canon) {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing path `{}` inside another user's home directory",
                    canon.display()
                )));
            }
        } else if self.is_other_users_home(&canon) {
            return Err(BerthError::InvalidArgument(format!(
                "refusing file `{}` inside another user's home directory",
                canon.display()
            )));
        }

        if check_sensitive_basename && meta.is_file() {
            if let Some(name) = canon.file_name().and_then(|n| n.to_str()) {
                if is_sensitive_filename(name) {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing sensitive file `{name}` for external attachment"
                    )));
                }
            }
        }

        Ok((canon, id, meta))
    }

    fn check_lexical_ancestors(&self, candidate: &Path) -> Result<(), BerthError> {
        // Normalize macOS `/System/Volumes/Data/...` firmlink spelling before checking lexical
        // ancestors so safe projects under `/System/Volumes/Data/Users/<me>/...` don't hit `/System`.
        let lexical = strip_macos_data_volume(candidate).unwrap_or_else(|| candidate.to_path_buf());
        for anc in lexical.ancestors() {
            if self.matches_subtree_denied_path(anc) || self.is_wsl_windows_sensitive_path(anc) {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing protected path `{}`",
                    candidate.display()
                )));
            }
            #[cfg(unix)]
            if let Ok(meta) = fs::symlink_metadata(anc) {
                let aid = FileIdentity::from_metadata(&meta);
                if self.subtree_denied_ids.contains(&aid) {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing protected path `{}`",
                        candidate.display()
                    )));
                }
            }
        }
        Ok(())
    }

    fn is_dynamic_exact_denied_root(&self, canon: &Path) -> bool {
        let p = strip_macos_data_volume(canon).unwrap_or_else(|| canon.to_path_buf());
        let my_home = strip_macos_data_volume(&self.real_home)
            .unwrap_or_else(|| self.real_home.clone());
        // Dynamically block `~/Library/CloudStorage` AND any immediate provider child
        // `~/Library/CloudStorage/<provider>` even if mounted after session startup.
        let cloud_root = my_home.join("Library").join("CloudStorage");
        if let Ok(rel) = strip_prefix_os(&p, &cloud_root) {
            if rel.components().count() <= 1 {
                return true;
            }
        }
        // On Linux/WSL, block `/mnt`, `/mnt/<drive>`, `/mnt/<drive>/Users`, and
        // `/mnt/<drive>/Users/<profile>` as direct roots (specific project subfolders allowed).
        if let Some((_drive, rest)) = parse_wsl_drvfs_path(&p) {
            let comps: Vec<_> = rest.components().collect();
            if comps.is_empty() {
                return true; // `/mnt/<drive>`
            }
            if comps[0].as_os_str().eq_ignore_ascii_case("Users") && comps.len() <= 2 {
                return true; // `/mnt/<drive>/Users` or `/mnt/<drive>/Users/<profile>`
            }
            if comps[0].as_os_str().eq_ignore_ascii_case("Windows") {
                return true;
            }
        }
        if p == Path::new("/mnt") {
            return true;
        }
        false
    }

    fn is_wsl_windows_sensitive_path(&self, p: &Path) -> bool {
        let Some((_drive, rest)) = parse_wsl_drvfs_path(p) else {
            return false;
        };
        let comps: Vec<String> = rest
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        if comps.is_empty() {
            return false;
        }
        if matches!(
            comps[0].as_str(),
            "windows" | "programdata" | "$recycle.bin" | "system volume information"
        ) {
            return true;
        }
        if comps[0] == "users" && comps.len() >= 3 {
            // Sensitive directories/files directly under `/mnt/<drive>/Users/<profile>/...`
            let sub = comps[2].as_str();
            if matches!(
                sub,
                "appdata"
                    | ".ssh"
                    | ".aws"
                    | ".gnupg"
                    | ".kube"
                    | ".docker"
                    | ".claude"
                    | ".codex"
                    | ".gemini"
                    | ".netrc"
                    | ".npmrc"
                    | ".pypirc"
                    | ".gitconfig"
                    | ".git-credentials"
                    | "ntuser.dat"
            ) {
                return true;
            }
            if sub == ".config" && comps.len() >= 4 {
                if matches!(comps[3].as_str(), "berth" | "gh" | "gcloud") {
                    return true;
                }
            }
            if sub == ".local" && comps.len() >= 5 && comps[3] == "state" {
                if matches!(comps[4].as_str(), "berth" | "ags") {
                    return true;
                }
            }
        }
        false
    }

    fn matches_exact_denied_path(&self, p: &Path) -> bool {
        let alt = strip_macos_data_volume(p);
        self.exact_only_denied_paths.iter().any(|d| {
            path_eq_os(p, d) || alt.as_deref().is_some_and(|a| path_eq_os(a, d))
        })
    }

    fn matches_subtree_denied_path(&self, p: &Path) -> bool {
        let alt = strip_macos_data_volume(p);
        self.subtree_denied_paths.iter().any(|d| {
            path_eq_os(p, d) || alt.as_deref().is_some_and(|a| path_eq_os(a, d))
        })
    }

    fn is_dynamic_protected_container(&self, canon_logical: &Path) -> bool {
        let my_home = strip_macos_data_volume(&self.real_home)
            .unwrap_or_else(|| self.real_home.clone());
        let cloud_root = my_home.join("Library").join("CloudStorage");
        if path_starts_with_os(&cloud_root, canon_logical) {
            return true;
        }
        if let Some((_drive, rest)) = parse_wsl_drvfs_path(canon_logical) {
            let comps: Vec<String> = rest
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
                .collect();
            if comps.is_empty() {
                return true; // `/mnt/<drive>` contains Windows & Users
            }
            if comps[0] == "users" {
                if comps.len() <= 2 {
                    return true; // `/mnt/<drive>/Users` or `/mnt/<drive>/Users/<profile>`
                }
                // Granting `/mnt/<drive>/Users/<profile>/.config`, `.local`, or `.local/state`
                // would expose dynamically protected `.config/{berth,gh,gcloud}` or `.local/state/{berth,ags}`!
                if comps.len() == 3 && matches!(comps[2].as_str(), ".config" | ".local") {
                    return true;
                }
                if comps.len() == 4 && comps[2] == ".local" && comps[3] == "state" {
                    return true;
                }
            }
        }
        false
    }

    fn is_other_users_home(&self, canon: &Path) -> bool {
        let p = strip_macos_data_volume(canon).unwrap_or_else(|| canon.to_path_buf());
        let my_home = strip_macos_data_volume(&self.real_home)
            .unwrap_or_else(|| self.real_home.clone());
        for parent in [Path::new("/Users"), Path::new("/home")] {
            if let Ok(rel) = strip_prefix_os(&p, parent) {
                if let Some(first) = rel.components().next() {
                    let candidate_home = parent.join(first.as_os_str());
                    if !path_eq_os(&candidate_home, &my_home)
                        && !first.as_os_str().eq_ignore_ascii_case("Shared")
                    {
                        return true;
                    }
                }
            }
        }
        // On WSL, block other Windows user profiles under `/mnt/<drive>/Users/<profile>`
        // FAIL-CLOSED: require BOTH drive AND active Windows username to match a verified
        // Windows profile root; if unverified, refuse ALL `/mnt/<drive>/Users/<profile>/...`.
        if let Some((drive, rest)) = parse_wsl_drvfs_path(&p) {
            let comps: Vec<_> = rest.components().collect();
            if comps.len() >= 2 && comps[0].as_os_str().eq_ignore_ascii_case("Users") {
                let target_win_user = comps[1].as_os_str().to_string_lossy();
                if matches!(
                    target_win_user.to_ascii_lowercase().as_str(),
                    "public" | "default" | "default user" | "all users"
                ) {
                    return true;
                }
                match detect_active_wsl_windows_profile(&self.real_home) {
                    Some((my_drive, my_win_user)) => {
                        if drive != my_drive || !target_win_user.eq_ignore_ascii_case(&my_win_user)
                        {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
        }
        false
    }
}

/// Computes the Git-aware, safe `WorkspacePlan` for `cwd`.
///
/// - Validates `cwd` against `SensitivePathGuard` first.
/// - Ascends parent directories within the same filesystem (`st_dev`) looking for a
///   `.git` directory, stopping before any denied/unsafe directory (e.g., `$HOME` or `/`).
/// - If `.git` is a pointer file (linked worktree / submodule / external gitdir), sets
///   `has_external_gitdir = true` and keeps `host_root` at that directory without mutating
///   host `.git` or exporting external `commondir`.
pub fn resolve_workspace_plan(
    cwd: &Path,
    guard: &SensitivePathGuard,
) -> Result<WorkspacePlan, BerthError> {
    let (canon_cwd, cwd_id, _) = guard.validate_existing_path(cwd, Some(true), false)?;

    let mut host_root = canon_cwd.clone();
    let mut host_root_id = cwd_id;
    let mut has_external_gitdir = false;

    let mut cur = canon_cwd.as_path();
    loop {
        let git_entry = cur.join(".git");
        if let Ok(git_meta) = fs::symlink_metadata(&git_entry) {
            if git_meta.is_dir() {
                // Only ascend to `cur` if `cur` itself passes the full guard!
                if let Ok((valid_root, valid_id, _)) =
                    guard.validate_existing_path(cur, Some(true), false)
                {
                    host_root = valid_root;
                    host_root_id = valid_id;
                }
                break;
            } else if git_meta.is_file() {
                if let Ok((valid_root, valid_id, _)) =
                    guard.validate_existing_path(cur, Some(true), false)
                {
                    host_root = valid_root;
                    host_root_id = valid_id;
                }
                has_external_gitdir = true;
                break;
            }
        }
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent == cur {
            break;
        }
        // Stop ascending across Unix mount boundaries or into protected directories.
        #[cfg(unix)]
        {
            let Ok(pmeta) = fs::metadata(parent) else {
                break;
            };
            if FileIdentity::from_metadata(&pmeta).dev != cwd_id.dev {
                break;
            }
        }
        if guard.validate_existing_path(parent, Some(true), false).is_err() {
            break;
        }
        cur = parent;
    }

    let relative_cwd = canon_cwd
        .strip_prefix(&host_root)
        .unwrap_or_else(|_| Path::new(""))
        .to_path_buf();
    let guest_cwd = if relative_cwd.as_os_str().is_empty() {
        PathBuf::from(GUEST_WORKSPACE_ROOT)
    } else {
        PathBuf::from(GUEST_WORKSPACE_ROOT).join(&relative_cwd)
    };
    let workspace_id = compute_workspace_id(&host_root);

    Ok(WorkspacePlan {
        host_root,
        host_root_id,
        host_cwd: canon_cwd,
        relative_cwd,
        guest_cwd,
        workspace_id,
        has_external_gitdir,
    })
}

pub fn compute_workspace_id(canonical_root: &Path) -> String {
    let mut h = Sha256::new();
    h.update(b"berth-ws-id-v1:");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        h.update(canonical_root.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        h.update(canonical_root.as_os_str().as_encoded_bytes());
    }
    let d = h.finalize();
    d[..6].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
struct PendingFilePublish {
    alias: String,
    source_identity: FileIdentity,
    tmp_path: PathBuf,
    staged_path: PathBuf,
    guest_path: String,
    generation: u64,
}

#[derive(Debug)]
struct PendingDirBatch {
    next_generation: u64,
    grants: Vec<(LiveDirGrant, FileIdentity)>,
    #[cfg(unix)]
    pinned_fds: Vec<(String, File)>,
    pending_files: Vec<PendingFilePublish>,
}

/// Host-only per-launch mount grant session.
///
/// Owns:
/// - `workspace`: the active project `/workspace` mapping (with kernel-pinned `O_DIRECTORY` fd on Unix)
/// - `staging_dir`: host-only `0700` dir (`<base>/exported`) exported RO as `/mnt/ext/_files`
/// - `staging_work_dir`: host-only `0700` sibling dir (`<base>/work`) NEVER exported to guest,
///   used for atomic file-snapshot writes before `fs::rename` into `staging_dir`
/// - `live_dirs`: committed directories exported as `/mnt/ext/d0001-<slug>` (with pinned `O_DIRECTORY` fds)
/// - `pending_dirs`: two-phase-commit staged directories + mixed-batch files awaiting VirtioFS commit ack
/// - monotonic `generation` counter synchronized with `berth-vz`
pub struct MountSession {
    pub workspace: WorkspacePlan,
    pub staging_dir: PathBuf,
    staging_work_dir: PathBuf,
    guard: SensitivePathGuard,
    #[cfg(unix)]
    #[allow(dead_code)]
    workspace_pinned_fd: Option<File>,
    live_dirs: Vec<LiveDirGrant>,
    live_by_identity: HashMap<FileIdentity, usize>,
    #[cfg(unix)]
    pinned_dir_fds: HashMap<String, File>,
    pending_dirs: Option<PendingDirBatch>,
    file_snapshots: Vec<FileSnapshotGrant>,
    file_by_identity: HashMap<FileIdentity, usize>,
    generation: u64,
}

impl MountSession {
    pub fn new(
        cwd: &Path,
        real_home: &Path,
        state_dir: &Path,
        config_dir: &Path,
        staging_base_dir: PathBuf,
    ) -> Result<Self, BerthError> {
        let staging_dir = staging_base_dir.join("exported");
        let staging_work_dir = staging_base_dir.join("work");
        fs::create_dir_all(&staging_dir)?;
        fs::create_dir_all(&staging_work_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&staging_base_dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&staging_dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&staging_work_dir, fs::Permissions::from_mode(0o700));
        }
        let guard = SensitivePathGuard::new(
            real_home,
            state_dir,
            config_dir,
            &[&staging_base_dir, &staging_dir, &staging_work_dir],
        );
        let workspace = resolve_workspace_plan(cwd, &guard)?;
        #[cfg(unix)]
        let workspace_pinned_fd = Some(open_directory_nofollow_verified(
            &workspace.host_root,
            workspace.host_root_id,
        )?);
        Ok(Self {
            workspace,
            staging_dir,
            staging_work_dir,
            guard,
            #[cfg(unix)]
            workspace_pinned_fd,
            live_dirs: Vec::new(),
            live_by_identity: HashMap::new(),
            #[cfg(unix)]
            pinned_dir_fds: HashMap::new(),
            pending_dirs: None,
            file_snapshots: Vec::new(),
            file_by_identity: HashMap::new(),
            generation: 0,
        })
    }

    /// On Linux, returns a kernel-FD-pinned `/proc/<pid>/fd/<fd>` path for `/workspace`,
    /// closing TOCTOU directory-swap races between authorization and `bwrap --bind`.
    #[cfg(target_os = "linux")]
    pub fn pinned_workspace_fd_path(&self) -> Option<PathBuf> {
        use std::os::fd::AsRawFd;
        self.workspace_pinned_fd.as_ref().map(|f| {
            PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), f.as_raw_fd()))
        })
    }

    /// On Linux, returns a kernel-FD-pinned `/proc/<pid>/fd/<fd>` path for a committed
    /// external directory grant, closing TOCTOU directory-swap races before `bwrap` runs.
    #[cfg(target_os = "linux")]
    pub fn pinned_dir_fd_path(&self, alias: &str) -> Option<PathBuf> {
        use std::os::fd::AsRawFd;
        self.pinned_dir_fds.get(alias).map(|f| {
            PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), f.as_raw_fd()))
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn live_dirs(&self) -> &[LiveDirGrant] {
        &self.live_dirs
    }

    pub fn file_snapshots(&self) -> &[FileSnapshotGrant] {
        &self.file_snapshots
    }

    /// Returns the committed directory map for `VZMultipleDirectoryShare` (`tag: "ext"`),
    /// always including `_files -> staging_dir (ro)` plus every committed live directory grant
    /// along with its verified `FileIdentity` (`st_dev`, `st_ino`).
    pub fn ext_share_entries(&self) -> BTreeMap<String, (PathBuf, bool, FileIdentity)> {
        let mut out = BTreeMap::new();
        let staging_id = fs::metadata(&self.staging_dir)
            .map(|m| FileIdentity::from_path_and_metadata(&self.staging_dir, &m))
            .unwrap_or_default();
        out.insert(
            GUEST_FILES_SUBDIR.to_string(),
            (self.staging_dir.clone(), true, staging_id),
        );
        for d in &self.live_dirs {
            out.insert(
                d.alias.clone(),
                (d.host_path.clone(), d.access.is_read_only(), d.identity),
            );
        }
        out
    }

    /// Returns the proposed directory map including any currently staged two-phase-commit
    /// directory grants awaiting `berth-vz` VirtioFS confirmation.
    pub fn ext_share_entries_with_pending(&self) -> BTreeMap<String, (PathBuf, bool, FileIdentity)> {
        let mut out = self.ext_share_entries();
        if let Some(pending) = &self.pending_dirs {
            for (d, id) in &pending.grants {
                out.insert(
                    d.alias.clone(),
                    (d.host_path.clone(), d.access.is_read_only(), *id),
                );
            }
        }
        out
    }

    /// Commits a pending two-phase VirtioFS directory grant batch (and atomically publishes
    /// any mixed-batch staged files from host-only `work/` into RO `exported/`) ONLY AFTER
    /// `berth-vz` confirms `VZMultipleDirectoryShare` updated cleanly on `vmQueue`.
    pub fn commit_pending(&mut self, expected_next_gen: u64) -> Result<(), BerthError> {
        let Some(pending) = self.pending_dirs.take() else {
            return Err(BerthError::InvalidArgument(
                "no pending directory mount commit".into(),
            ));
        };
        if pending.next_generation != expected_next_gen {
            self.pending_dirs = Some(pending);
            return Err(BerthError::InvalidArgument(format!(
                "commit generation mismatch: expected {expected_next_gen}"
            )));
        }
        // Atomically rename any mixed-batch staged files into `exported/` before committing.
        let mut published: Vec<(FileIdentity, PathBuf)> = Vec::new();
        let prev_file_len = self.file_snapshots.len();
        for pf in pending.pending_files {
            if let Err(err) = fs::rename(&pf.tmp_path, &pf.staged_path) {
                let _ = fs::remove_file(&pf.tmp_path);
                self.file_snapshots.truncate(prev_file_len);
                for (fid, sp) in published {
                    self.file_by_identity.remove(&fid);
                    let _ = fs::remove_file(sp);
                }
                return Err(err.into());
            }
            published.push((pf.source_identity, pf.staged_path.clone()));
            let idx = self.file_snapshots.len();
            self.file_snapshots.push(FileSnapshotGrant {
                alias: pf.alias,
                source_identity: pf.source_identity,
                staged_path: pf.staged_path,
                guest_path: pf.guest_path,
                generation: pf.generation,
            });
            self.file_by_identity.insert(pf.source_identity, idx);
        }
        for (grant, id) in pending.grants {
            let idx = self.live_dirs.len();
            self.live_dirs.push(grant);
            self.live_by_identity.insert(id, idx);
        }
        #[cfg(unix)]
        for (alias, fd) in pending.pinned_fds {
            self.pinned_dir_fds.insert(alias, fd);
        }
        self.generation = expected_next_gen;
        Ok(())
    }

    /// Aborts a pending two-phase VirtioFS directory grant batch (deletes any un-published
    /// mixed-batch temp files in host-only `work/` and rolls back without advancing state).
    pub fn abort_pending(&mut self) {
        if let Some(pending) = self.pending_dirs.take() {
            for pf in pending.pending_files {
                let _ = fs::remove_file(&pf.tmp_path);
            }
        }
    }

    /// Explicitly grants a host directory from an OS path (rejecting non-UTF-8 paths fail-closed
    /// and preserving exact whitespace without trimming so trailing-space sibling directories can
    /// NEVER be substituted!).
    pub fn grant_directory_os(
        &mut self,
        raw_path: &Path,
        access: MountAccess,
    ) -> Result<GrantOutcome, BerthError> {
        let s = raw_path.to_str().ok_or_else(|| {
            BerthError::InvalidArgument(
                "non-UTF-8 host mount path is refused (lossy conversion forbidden)".into(),
            )
        })?;
        let outcome = self.stage_directory_grant_opts(s, access, false)?;
        if outcome.needs_vz_update {
            self.commit_pending(outcome.next_generation)?;
        }
        Ok(outcome)
    }

    /// Explicitly grants a host directory at startup (`--mount`, `--ro-mount`, `--add-dir`),
    /// committing immediately (before VM/bwrap launches).
    pub fn grant_directory(
        &mut self,
        raw_path: &str,
        access: MountAccess,
    ) -> Result<GrantOutcome, BerthError> {
        let outcome = self.stage_directory_grant(raw_path, access)?;
        if outcome.needs_vz_update {
            self.commit_pending(outcome.next_generation)?;
        }
        Ok(outcome)
    }

    /// Resolves and immediately commits a single host path (used in tests and non-two-phase callers).
    pub fn resolve_or_grant_host_path(
        &mut self,
        raw_path: &str,
        dir_access: MountAccess,
        enforce_sensitive_basename: bool,
    ) -> Result<GrantOutcome, BerthError> {
        let outcome = self.stage_attach(raw_path, dir_access, enforce_sensitive_basename)?;
        if outcome.needs_vz_update {
            self.commit_pending(outcome.next_generation)?;
        }
        Ok(outcome)
    }

    /// Two-phase live attach: validates target, snapshots single files atomically via
    /// host-only `staging_work_dir`, or stages a new directory grant in `pending_dirs`
    /// (caller MUST call `commit_pending` or `abort_pending` when `needs_vz_update` is true).
    pub fn stage_attach(
        &mut self,
        raw_path: &str,
        dir_access: MountAccess,
        enforce_sensitive_basename: bool,
    ) -> Result<GrantOutcome, BerthError> {
        if self.pending_dirs.is_some() {
            return Err(BerthError::InvalidArgument(
                "previous directory mount update is still uncommitted".into(),
            ));
        }
        let expanded = self.guard.parse_and_expand_host_input(raw_path)?;

        // Check lexical workspace containment (strictly rejects workspace symlink escapes!).
        if let Some(ws_guest) = self.map_inside_workspace(&expanded, None, Some(dir_access))? {
            return Ok(GrantOutcome {
                guest_path: ws_guest,
                access: MountAccess::Rw,
                is_file_snapshot: false,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            });
        }

        let (canon, id, meta) =
            self.guard
                .validate_existing_path(&expanded, None, enforce_sensitive_basename)?;

        // Re-check canonical workspace containment (e.g., external symlink pointing into workspace).
        if let Ok(rel) = canon.strip_prefix(&self.workspace.host_root) {
            if meta.is_dir() && dir_access == MountAccess::Ro {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing read-only mount `{}` inside writable /workspace",
                    canon.display()
                )));
            }
            let guest_path = if rel.as_os_str().is_empty() {
                GUEST_WORKSPACE_ROOT.to_string()
            } else {
                format!("{GUEST_WORKSPACE_ROOT}/{}", rel.to_string_lossy())
            };
            return Ok(GrantOutcome {
                guest_path,
                access: MountAccess::Rw,
                is_file_snapshot: false,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            });
        }

        if meta.is_file() {
            self.grant_file_snapshot_validated(&canon, id)
        } else {
            self.stage_live_directory_validated(&canon, id, dir_access)
        }
    }

    fn stage_directory_grant(
        &mut self,
        raw_path: &str,
        access: MountAccess,
    ) -> Result<GrantOutcome, BerthError> {
        self.stage_directory_grant_opts(raw_path, access, true)
    }

    fn stage_directory_grant_opts(
        &mut self,
        raw_path: &str,
        access: MountAccess,
        trim_ws: bool,
    ) -> Result<GrantOutcome, BerthError> {
        if self.pending_dirs.is_some() {
            return Err(BerthError::InvalidArgument(
                "previous directory mount update is still uncommitted".into(),
            ));
        }
        let expanded = self.guard.parse_and_expand_host_input_opts(raw_path, trim_ws)?;
        if let Some(ws_guest) = self.map_inside_workspace(&expanded, Some(true), Some(access))? {
            return Ok(GrantOutcome {
                guest_path: ws_guest,
                access: MountAccess::Rw,
                is_file_snapshot: false,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            });
        }
        let (canon, id, _) = self.guard.validate_existing_path(&expanded, Some(true), false)?;
        if let Ok(rel) = canon.strip_prefix(&self.workspace.host_root) {
            if access == MountAccess::Ro {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing read-only mount `{}` inside writable /workspace",
                    canon.display()
                )));
            }
            let guest_path = if rel.as_os_str().is_empty() {
                GUEST_WORKSPACE_ROOT.to_string()
            } else {
                format!("{GUEST_WORKSPACE_ROOT}/{}", rel.to_string_lossy())
            };
            return Ok(GrantOutcome {
                guest_path,
                access: MountAccess::Rw,
                is_file_snapshot: false,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            });
        }
        self.stage_live_directory_validated(&canon, id, access)
    }

    fn map_inside_workspace(
        &self,
        expanded: &Path,
        expect_dir: Option<bool>,
        requested_dir_access: Option<MountAccess>,
    ) -> Result<Option<String>, BerthError> {
        let ws_clean = strip_macos_data_volume(&self.workspace.host_root)
            .unwrap_or_else(|| self.workspace.host_root.clone());

        // Walk EVERY component incrementally (resolving symlinks up to 40 hops) and track
        // whether resolution EVER entered `/workspace`. If any subsequent symlink or step
        // transitions from inside `/workspace` to outside `/workspace`, reject immediately
        // regardless of alternate prefix spellings (`/var` vs `/private/var`, symlinked
        // parents, or multi-hop intermediate links)!
        let mut entered_workspace = false;
        let mut resolved = PathBuf::new();
        let mut remaining: std::collections::VecDeque<std::ffi::OsString> = expanded
            .components()
            .filter_map(|c| match c {
                Component::RootDir => {
                    resolved = PathBuf::from("/");
                    None
                }
                Component::Normal(seg) => Some(seg.to_os_string()),
                Component::CurDir => None,
                Component::ParentDir => Some(std::ffi::OsString::from("..")),
                Component::Prefix(p) => {
                    resolved.push(p.as_os_str());
                    None
                }
            })
            .collect();
        let mut symlink_hops = 0usize;
        let mut missing_tail = false;
        // Track remaining components belonging to an in-flight symlink expansion so an
        // absolute symlink pointing BACK into `/workspace` (e.g. `/workspace/a -> /workspace/b`)
        // is not falsely rejected while stepping through `/` -> `/Users` -> `/workspace/b`!
        let mut symlink_expansion_remaining = 0usize;

        while let Some(seg) = remaining.pop_front() {
            if seg == ".." {
                resolved.pop();
            } else {
                resolved.push(&seg);
            }
            if symlink_expansion_remaining > 0 {
                symlink_expansion_remaining -= 1;
            }
            if !missing_tail {
                match fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        symlink_hops += 1;
                        if symlink_hops > 40 {
                            return Err(BerthError::InvalidArgument(format!(
                                "too many symlink hops resolving `{}`",
                                expanded.display()
                            )));
                        }
                        let target = fs::read_link(&resolved).map_err(|e| {
                            BerthError::InvalidArgument(format!(
                                "cannot read symlink `{}`: {e}",
                                resolved.display()
                            ))
                        })?;
                        if target.is_absolute() {
                            resolved = PathBuf::from("/");
                        } else {
                            resolved.pop();
                        }
                        let target_segs: Vec<std::ffi::OsString> = target
                            .components()
                            .filter_map(|c| match c {
                                Component::Normal(s) => Some(s.to_os_string()),
                                Component::ParentDir => Some(std::ffi::OsString::from("..")),
                                _ => None,
                            })
                            .collect();
                        symlink_expansion_remaining += target_segs.len();
                        for ts in target_segs.into_iter().rev() {
                            remaining.push_front(ts);
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        missing_tail = true;
                    }
                }
            }
            if symlink_expansion_remaining == 0 {
                let cur_clean =
                    strip_macos_data_volume(&resolved).unwrap_or_else(|| resolved.clone());
                let currently_inside = strip_prefix_os(&cur_clean, &ws_clean).is_ok();
                if currently_inside {
                    entered_workspace = true;
                } else if entered_workspace {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing symlink `{}` inside workspace that escapes outside /workspace",
                        expanded.display()
                    )));
                }
            }
        }

        let final_clean = strip_macos_data_volume(&resolved).unwrap_or_else(|| resolved.clone());
        let matched_rel = strip_prefix_os(&final_clean, &ws_clean).ok();
        if let Some(rel) = matched_rel {
            let meta_exists = !missing_tail && fs::symlink_metadata(&resolved).is_ok();
            if expect_dir.is_some() && !meta_exists {
                return Err(BerthError::InvalidArgument(format!(
                    "host path `{}` does not exist",
                    expanded.display()
                )));
            }
            if meta_exists {
                let canon = fs::canonicalize(&resolved).map_err(|e| {
                    BerthError::InvalidArgument(format!(
                        "cannot resolve workspace path `{}`: {e}",
                        expanded.display()
                    ))
                })?;
                if canon.strip_prefix(&self.workspace.host_root).is_err() {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing symlink `{}` inside workspace that escapes outside /workspace",
                        expanded.display()
                    )));
                }
                let meta = fs::metadata(&canon)?;
                if let Some(want_dir) = expect_dir {
                    if want_dir && !meta.is_dir() {
                        return Err(BerthError::InvalidArgument(format!(
                            "expected directory, got file `{}`",
                            canon.display()
                        )));
                    }
                    if !want_dir && !meta.is_file() {
                        return Err(BerthError::InvalidArgument(format!(
                            "expected regular file, got directory `{}`",
                            canon.display()
                        )));
                    }
                }
                if meta.is_dir() && requested_dir_access == Some(MountAccess::Ro) {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing read-only mount `{}` inside writable /workspace",
                        canon.display()
                    )));
                }
                let canon_rel = canon
                    .strip_prefix(&self.workspace.host_root)
                    .unwrap_or(rel.as_path());
                let guest_path = if canon_rel.as_os_str().is_empty() {
                    GUEST_WORKSPACE_ROOT.to_string()
                } else {
                    format!("{GUEST_WORKSPACE_ROOT}/{}", canon_rel.to_string_lossy())
                };
                return Ok(Some(guest_path));
            }
            let guest_path = if rel.as_os_str().is_empty() {
                GUEST_WORKSPACE_ROOT.to_string()
            } else {
                format!("{GUEST_WORKSPACE_ROOT}/{}", rel.to_string_lossy())
            };
            return Ok(Some(guest_path));
        }
        Ok(None)
    }

    fn validate_file_for_snapshot(
        &self,
        canon: &Path,
        expected_id: FileIdentity,
    ) -> Result<(File, FileIdentity), BerthError> {
        let src = open_regular_nofollow(canon)?;
        let fmeta = src.metadata()?;
        if !fmeta.is_file() {
            return Err(BerthError::InvalidArgument(format!(
                "refusing non-regular file `{}`",
                canon.display()
            )));
        }
        let open_id = FileIdentity::from_path_and_metadata(canon, &fmeta);
        if open_id != expected_id {
            return Err(BerthError::InvalidArgument(format!(
                "file `{}` changed during authorization (TOCTOU rejected)",
                canon.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if fmeta.nlink() > 1 {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing multiply-linked external file `{}` (nlink={})",
                    canon.display(),
                    fmeta.nlink()
                )));
            }
        }
        if fmeta.len() > MAX_SNAPSHOT_FILE_BYTES {
            return Err(BerthError::InvalidArgument(format!(
                "file `{}` exceeds snapshot limit ({} > {} bytes)",
                canon.display(),
                fmeta.len(),
                MAX_SNAPSHOT_FILE_BYTES
            )));
        }
        Ok((src, open_id))
    }

    /// Stages a validated single file into host-only `staging_work_dir` (NEVER visible to guest
    /// until renamed into `staging_dir` on publish/commit). Returns `None` if already cached.
    fn stage_file_into_work_dir(
        &self,
        canon: &Path,
        expected_id: FileIdentity,
        seq: usize,
        generation: u64,
    ) -> Result<Result<PendingFilePublish, String>, BerthError> {
        let (mut src, open_id) = self.validate_file_for_snapshot(canon, expected_id)?;
        if let Some(&idx) = self.file_by_identity.get(&open_id) {
            return Ok(Err(self.file_snapshots[idx].guest_path.clone()));
        }

        let base_name = canon
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let alias = format!("f{seq:04}-{}", sanitize_alias_filename(base_name));
        let staged_path = self.staging_dir.join(&alias);
        let tmp_path = self
            .staging_work_dir
            .join(format!(".{alias}-{}.tmp", uuid::Uuid::new_v4().simple()));
        struct TempCleanup(Option<PathBuf>);
        impl Drop for TempCleanup {
            fn drop(&mut self) {
                if let Some(p) = self.0.take() {
                    let _ = fs::remove_file(p);
                }
            }
        }
        let mut cleanup = TempCleanup(Some(tmp_path.clone()));

        let mut dst = File::create(&tmp_path)?;
        let mut total: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > MAX_SNAPSHOT_FILE_BYTES {
                drop(dst);
                return Err(BerthError::InvalidArgument(format!(
                    "file `{}` grew beyond snapshot limit during copy",
                    canon.display()
                )));
            }
            dst.write_all(&buf[..n])?;
        }
        dst.sync_all()?;
        drop(dst);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o444));
        }
        cleanup.0 = None;
        let guest_path = format!("{GUEST_EXT_ROOT}/{GUEST_FILES_SUBDIR}/{alias}");
        Ok(Ok(PendingFilePublish {
            alias,
            source_identity: open_id,
            tmp_path,
            staged_path,
            guest_path,
            generation,
        }))
    }

    fn grant_file_snapshot_validated(
        &mut self,
        canon: &Path,
        expected_id: FileIdentity,
    ) -> Result<GrantOutcome, BerthError> {
        let seq = self.file_snapshots.len() + 1;
        match self.stage_file_into_work_dir(canon, expected_id, seq, self.generation)? {
            Err(existing_guest) => Ok(GrantOutcome {
                guest_path: existing_guest,
                access: MountAccess::Ro,
                is_file_snapshot: true,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            }),
            Ok(pf) => {
                if let Err(err) = fs::rename(&pf.tmp_path, &pf.staged_path) {
                    let _ = fs::remove_file(&pf.tmp_path);
                    return Err(err.into());
                }
                let guest_path = pf.guest_path.clone();
                let idx = self.file_snapshots.len();
                self.file_snapshots.push(FileSnapshotGrant {
                    alias: pf.alias,
                    source_identity: pf.source_identity,
                    staged_path: pf.staged_path,
                    guest_path: pf.guest_path,
                    generation: pf.generation,
                });
                self.file_by_identity.insert(pf.source_identity, idx);
                Ok(GrantOutcome {
                    guest_path,
                    access: MountAccess::Ro,
                    is_file_snapshot: true,
                    needs_vz_update: false,
                    base_generation: self.generation,
                    next_generation: self.generation,
                })
            }
        }
    }

    fn check_live_dir_against_existing(
        &self,
        canon: &Path,
        id: FileIdentity,
        access: MountAccess,
    ) -> Result<Option<String>, BerthError> {
        // Treat `/workspace` itself as an existing RW grant in BOTH directions by BOTH path
        // prefix AND ancestor `(st_dev, st_ino)` so a bind/alternate-mount path to the same
        // parent inode of `/workspace` can NEVER evade the RO-parent rejection!
        let ws_ancestor_id_hit = {
            #[cfg(unix)]
            {
                self.workspace.host_root.ancestors().any(|anc| {
                    fs::metadata(anc)
                        .map(|m| FileIdentity::from_metadata(&m) == id)
                        .unwrap_or(false)
                }) || canon.ancestors().any(|anc| {
                    fs::metadata(anc)
                        .map(|m| FileIdentity::from_metadata(&m) == self.workspace.host_root_id)
                        .unwrap_or(false)
                })
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if (self.workspace.host_root.strip_prefix(canon).is_ok() || ws_ancestor_id_hit)
            && access == MountAccess::Ro
        {
            return Err(BerthError::InvalidArgument(format!(
                "refusing read-only mount `{}` because it overlaps writable /workspace (`{}`)",
                canon.display(),
                self.workspace.host_root.display()
            )));
        }

        // Exact same directory already granted? Reject ANY conflicting permission (RO->RW OR RW->RO)!
        if let Some(&idx) = self.live_by_identity.get(&id) {
            let existing = &self.live_dirs[idx];
            if existing.access != access {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing conflicting {} grant on already-granted {} directory `{}`",
                    access.as_str(),
                    existing.access.as_str(),
                    canon.display()
                )));
            }
            return Ok(Some(format!("{GUEST_EXT_ROOT}/{}", existing.alias)));
        }

        // Check parent/child overlap against ALL existing committed grants (both directions!).
        for existing in &self.live_dirs {
            if let Ok(rel) = canon.strip_prefix(&existing.host_path) {
                if existing.access != access {
                    return Err(BerthError::InvalidArgument(format!(
                        "refusing conflicting {} grant `{}` overlapping existing {} grant `{}`",
                        access.as_str(),
                        canon.display(),
                        existing.access.as_str(),
                        existing.host_path.display()
                    )));
                }
                let guest_path = if rel.as_os_str().is_empty() {
                    format!("{GUEST_EXT_ROOT}/{}", existing.alias)
                } else {
                    format!("{GUEST_EXT_ROOT}/{}/{}", existing.alias, rel.to_string_lossy())
                };
                return Ok(Some(guest_path));
            }
            if existing.host_path.strip_prefix(canon).is_ok() && existing.access != access {
                return Err(BerthError::InvalidArgument(format!(
                    "refusing overlapping directory grant `{}` ({}) conflicting with existing child `{}` ({})",
                    canon.display(),
                    access.as_str(),
                    existing.host_path.display(),
                    existing.access.as_str()
                )));
            }
        }
        Ok(None)
    }

    fn stage_live_directory_validated(
        &mut self,
        canon: &Path,
        id: FileIdentity,
        access: MountAccess,
    ) -> Result<GrantOutcome, BerthError> {
        if let Some(existing_guest) = self.check_live_dir_against_existing(canon, id, access)? {
            return Ok(GrantOutcome {
                guest_path: existing_guest,
                access,
                is_file_snapshot: false,
                needs_vz_update: false,
                base_generation: self.generation,
                next_generation: self.generation,
            });
        }

        #[cfg(unix)]
        let pinned_fd = open_directory_nofollow_verified(canon, id)?;

        let base_generation = self.generation;
        let next_generation = base_generation + 1;
        let seq = self.live_dirs.len() + 1;
        let base_name = canon
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("dir");
        let alias = format!("d{seq:04}-{}", sanitize_alias_slug(base_name));
        let grant = LiveDirGrant {
            alias: alias.clone(),
            host_path: canon.to_path_buf(),
            access,
            generation: next_generation,
            identity: id,
        };
        self.pending_dirs = Some(PendingDirBatch {
            next_generation,
            grants: vec![(grant, id)],
            #[cfg(unix)]
            pinned_fds: vec![(alias.clone(), pinned_fd)],
            pending_files: Vec::new(),
        });

        Ok(GrantOutcome {
            guest_path: format!("{GUEST_EXT_ROOT}/{alias}"),
            access,
            is_file_snapshot: false,
            needs_vz_update: true,
            base_generation,
            next_generation,
        })
    }

    /// Rewrites documented `--add-dir <PATH>` / `--add-dir=<PATH>` startup CLI flags
    /// for `claude`, `codex`, and `agy` into RO guest grants (`/workspace/...` or `/mnt/ext/...`).
    /// Stops option scanning at a bare `--` separator so prompt text is preserved untouched.
    /// Fails closed on a trailing `--add-dir` missing its directory argument.
    pub fn rewrite_startup_add_dirs(
        &mut self,
        agent: &str,
        agent_args: &[String],
    ) -> Result<Vec<String>, BerthError> {
        if !matches!(agent, "claude" | "codex" | "agy") {
            return Ok(agent_args.to_vec());
        }
        let mut out = Vec::with_capacity(agent_args.len());
        let mut i = 0;
        let mut stop_opts = false;
        while i < agent_args.len() {
            let arg = &agent_args[i];
            if stop_opts {
                out.push(arg.clone());
                i += 1;
                continue;
            }
            if arg == "--" {
                stop_opts = true;
                out.push(arg.clone());
                i += 1;
                continue;
            }
            if arg == "--add-dir" {
                if i + 1 >= agent_args.len() {
                    return Err(BerthError::InvalidArgument(
                        "`--add-dir` requires a directory path argument".into(),
                    ));
                }
                let raw = &agent_args[i + 1];
                let grant = self.grant_directory(raw, MountAccess::Ro)?;
                out.push("--add-dir".into());
                out.push(grant.guest_path);
                i += 2;
                continue;
            }
            if let Some(raw) = arg.strip_prefix("--add-dir=") {
                if raw.trim().is_empty() {
                    return Err(BerthError::InvalidArgument(
                        "`--add-dir=` requires a non-empty directory path argument".into(),
                    ));
                }
                let grant = self.grant_directory(raw, MountAccess::Ro)?;
                out.push(format!("--add-dir={}", grant.guest_path));
                i += 1;
                continue;
            }
            out.push(arg.clone());
            i += 1;
        }
        Ok(out)
    }

    /// Atomic host-paste rewriter (convenience wrapper that immediately commits staged dirs).
    pub fn rewrite_host_paste(&mut self, pasted: &str) -> PasteRewriteOutcome {
        let outcome = self.stage_host_paste(pasted);
        if let PasteRewriteOutcome::Rewritten {
            needs_vz_update: true,
            next_generation,
            ..
        } = &outcome
        {
            if let Err(e) = self.commit_pending(*next_generation) {
                self.abort_pending();
                return PasteRewriteOutcome::Blocked {
                    reason: e.to_string(),
                };
            }
        }
        outcome
    }

    /// Two-phase atomic host-side bracketed paste / Finder drag-and-drop processor:
    /// - Phase 1 validates EVERY token strictly (syntax, quote matching, workspace escape,
    ///   sensitive guard, file `nlink==1`/size, existing & intra-batch dir conflicts) with
    ///   ZERO filesystem writes and ZERO state mutations.
    /// - Phase 2 publishes single-file snapshots atomically (with rollback on any I/O error)
    ///   and stages any new directory grants in `pending_dirs` awaiting `berth-vz` commit!
    pub fn stage_host_paste(&mut self, pasted: &str) -> PasteRewriteOutcome {
        if self.pending_dirs.is_some() {
            return PasteRewriteOutcome::Blocked {
                reason: "previous directory mount update is still uncommitted".into(),
            };
        }
        let trimmed = pasted.trim();
        if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
            return PasteRewriteOutcome::Unchanged;
        }
        let tokens = match split_finder_paste_tokens(trimmed) {
            Ok(t) => t,
            Err(err) => {
                // Fail closed if malformed quoting looks like ANY host filesystem path drop.
                let ws_prefix = self.workspace.host_root.to_string_lossy();
                let looks_path_like = trimmed.contains("/Users/")
                    || trimmed.contains("/users/")
                    || trimmed.contains("/Volumes/")
                    || trimmed.contains("/System/Volumes/Data")
                    || trimmed.contains("/private/")
                    || trimmed.contains("/home/")
                    || trimmed.contains("/mnt/")
                    || trimmed.contains("/tmp/")
                    || trimmed.contains("/var/")
                    || trimmed.starts_with("~/")
                    || trimmed.starts_with("'~/")
                    || trimmed.starts_with("\"~/")
                    || trimmed.starts_with("'/")
                    || trimmed.starts_with("\"/")
                    || (!ws_prefix.is_empty() && trimmed.contains(ws_prefix.as_ref()));
                if looks_path_like {
                    return PasteRewriteOutcome::Blocked {
                        reason: err.to_string(),
                    };
                }
                return PasteRewriteOutcome::Unchanged;
            }
        };
        if tokens.is_empty() {
            return PasteRewriteOutcome::Unchanged;
        }
        if !tokens.iter().all(|t| self.looks_like_host_path(t)) {
            return PasteRewriteOutcome::Unchanged;
        }

        // Phase 1: Validate EVERY token with ZERO mutation!
        enum PlannedToken {
            Workspace(String),
            File { canon: PathBuf, id: FileIdentity },
            ExistingDir(String),
            NewDir { alias: String },
        }

        let mut planned = Vec::with_capacity(tokens.len());
        let mut batch_new_dirs: Vec<(PathBuf, FileIdentity, String)> = Vec::new();
        let next_gen_if_dirs = self.generation + 1;

        for tok in &tokens {
            let expanded = match self.guard.parse_and_expand_host_input(tok) {
                Ok(p) => p,
                Err(e) => return PasteRewriteOutcome::Blocked { reason: e.to_string() },
            };
            match self.map_inside_workspace(&expanded, None, None) {
                Ok(Some(ws_guest)) => {
                    planned.push(PlannedToken::Workspace(ws_guest));
                    continue;
                }
                Ok(None) => {}
                Err(e) => return PasteRewriteOutcome::Blocked { reason: e.to_string() },
            }

            let (canon, id, meta) = match self.guard.validate_existing_path(&expanded, None, true) {
                Ok(v) => v,
                Err(e) => return PasteRewriteOutcome::Blocked { reason: e.to_string() },
            };
            if let Ok(rel) = canon.strip_prefix(&self.workspace.host_root) {
                let ws_guest = if rel.as_os_str().is_empty() {
                    GUEST_WORKSPACE_ROOT.to_string()
                } else {
                    format!("{GUEST_WORKSPACE_ROOT}/{}", rel.to_string_lossy())
                };
                planned.push(PlannedToken::Workspace(ws_guest));
                continue;
            }

            if meta.is_file() {
                if let Err(e) = self.validate_file_for_snapshot(&canon, id) {
                    return PasteRewriteOutcome::Blocked { reason: e.to_string() };
                }
                planned.push(PlannedToken::File { canon, id });
            } else {
                match self.check_live_dir_against_existing(&canon, id, MountAccess::Ro) {
                    Ok(Some(existing_guest)) => {
                        planned.push(PlannedToken::ExistingDir(existing_guest));
                    }
                    Ok(None) => {
                        // Reuse if already planned in this same batch (exact or child).
                        let mut reused: Option<String> = None;
                        for (b_path, b_id, b_alias) in &batch_new_dirs {
                            if *b_id == id {
                                reused = Some(format!("{GUEST_EXT_ROOT}/{b_alias}"));
                                break;
                            }
                            if let Ok(rel) = canon.strip_prefix(b_path) {
                                reused = Some(format!(
                                    "{GUEST_EXT_ROOT}/{b_alias}/{}",
                                    rel.to_string_lossy()
                                ));
                                break;
                            }
                        }
                        if let Some(g) = reused {
                            planned.push(PlannedToken::ExistingDir(g));
                        } else {
                            let seq = self.live_dirs.len() + batch_new_dirs.len() + 1;
                            let base_name = canon
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("dir");
                            let alias = format!("d{seq:04}-{}", sanitize_alias_slug(base_name));
                            batch_new_dirs.push((canon.clone(), id, alias.clone()));
                            planned.push(PlannedToken::NewDir { alias });
                        }
                    }
                    Err(e) => return PasteRewriteOutcome::Blocked { reason: e.to_string() },
                }
            }
        }

        // Phase 2: All tokens passed validation!
        // Stage ALL newly copied files privately in `staging_work_dir` FIRST (ZERO visibility
        // in `exported/` while copying or while a mixed dir update awaits VZ commit!), and open
        // pinned `O_DIRECTORY | O_NOFOLLOW` fds for every new directory grant.
        let start_gen = self.generation;
        let has_new_dirs = !batch_new_dirs.is_empty();
        let target_gen = if has_new_dirs { next_gen_if_dirs } else { start_gen };
        let mut staged_files: Vec<PendingFilePublish> = Vec::new();
        let mut staged_file_by_id: HashMap<FileIdentity, String> = HashMap::new();
        let mut rewritten_tokens = Vec::with_capacity(planned.len());
        let mut notices = Vec::new();

        let cleanup_staged_files = |files: &[PendingFilePublish]| {
            for pf in files {
                let _ = fs::remove_file(&pf.tmp_path);
            }
        };

        for item in planned {
            match item {
                PlannedToken::Workspace(g) => {
                    notices.push(format!("{g} [workspace]"));
                    rewritten_tokens.push(shell_quote_if_needed(&g));
                }
                PlannedToken::ExistingDir(g) => {
                    notices.push(format!("{g} [dir (ro)]"));
                    rewritten_tokens.push(shell_quote_if_needed(&g));
                }
                PlannedToken::NewDir { alias, .. } => {
                    let g = format!("{GUEST_EXT_ROOT}/{alias}");
                    notices.push(format!("{g} [dir (ro)]"));
                    rewritten_tokens.push(shell_quote_if_needed(&g));
                }
                PlannedToken::File { canon, id } => {
                    if let Some(existing_in_batch) = staged_file_by_id.get(&id) {
                        notices.push(format!("{existing_in_batch} [file snapshot (ro)]"));
                        rewritten_tokens.push(shell_quote_if_needed(existing_in_batch));
                        continue;
                    }
                    let seq = self.file_snapshots.len() + staged_files.len() + 1;
                    match self.stage_file_into_work_dir(&canon, id, seq, target_gen) {
                        Ok(Err(existing_guest)) => {
                            notices.push(format!("{existing_guest} [file snapshot (ro)]"));
                            rewritten_tokens.push(shell_quote_if_needed(&existing_guest));
                        }
                        Ok(Ok(pf)) => {
                            let g = pf.guest_path.clone();
                            staged_file_by_id.insert(pf.source_identity, g.clone());
                            staged_files.push(pf);
                            notices.push(format!("{g} [file snapshot (ro)]"));
                            rewritten_tokens.push(shell_quote_if_needed(&g));
                        }
                        Err(err) => {
                            cleanup_staged_files(&staged_files);
                            return PasteRewriteOutcome::Blocked {
                                reason: err.to_string(),
                            };
                        }
                    }
                }
            }
        }

        let (needs_vz_update, next_generation) = if has_new_dirs {
            #[cfg(unix)]
            let mut pinned_fds = Vec::with_capacity(batch_new_dirs.len());
            #[cfg(unix)]
            for (host_path, id, alias) in &batch_new_dirs {
                match open_directory_nofollow_verified(host_path, *id) {
                    Ok(fd) => pinned_fds.push((alias.clone(), fd)),
                    Err(err) => {
                        cleanup_staged_files(&staged_files);
                        return PasteRewriteOutcome::Blocked {
                            reason: err.to_string(),
                        };
                    }
                }
            }
            let grants = batch_new_dirs
                .into_iter()
                .map(|(host_path, id, alias)| {
                    (
                        LiveDirGrant {
                            alias,
                            host_path,
                            access: MountAccess::Ro,
                            generation: next_gen_if_dirs,
                            identity: id,
                        },
                        id,
                    )
                })
                .collect();
            self.pending_dirs = Some(PendingDirBatch {
                next_generation: next_gen_if_dirs,
                grants,
                #[cfg(unix)]
                pinned_fds,
                pending_files: staged_files,
            });
            (true, next_gen_if_dirs)
        } else {
            // File-only batch: ALL copies succeeded privately in `work/`; now publish as a unit.
            let prev_len = self.file_snapshots.len();
            let mut published: Vec<(FileIdentity, PathBuf)> = Vec::new();
            for pf in staged_files {
                if let Err(err) = fs::rename(&pf.tmp_path, &pf.staged_path) {
                    let _ = fs::remove_file(&pf.tmp_path);
                    self.file_snapshots.truncate(prev_len);
                    for (fid, sp) in published {
                        self.file_by_identity.remove(&fid);
                        let _ = fs::remove_file(sp);
                    }
                    return PasteRewriteOutcome::Blocked {
                        reason: err.to_string(),
                    };
                }
                published.push((pf.source_identity, pf.staged_path.clone()));
                let idx = self.file_snapshots.len();
                self.file_snapshots.push(FileSnapshotGrant {
                    alias: pf.alias,
                    source_identity: pf.source_identity,
                    staged_path: pf.staged_path,
                    guest_path: pf.guest_path,
                    generation: pf.generation,
                });
                self.file_by_identity.insert(pf.source_identity, idx);
            }
            (false, start_gen)
        };

        let mut rewritten = rewritten_tokens.join(" ");
        if pasted.ends_with(' ') {
            rewritten.push(' ');
        }

        PasteRewriteOutcome::Rewritten {
            text: rewritten,
            notices,
            needs_vz_update,
            base_generation: start_gen,
            next_generation,
        }
    }

    fn looks_like_host_path(&self, token: &str) -> bool {
        if token.starts_with("~/") || token == "~" {
            return true;
        }
        if matches!(
            token,
            "/Users" | "/users" | "/Volumes" | "/System/Volumes/Data" | "/System/Volumes/Data/Users"
        ) || token.starts_with("/Users/")
            || token.starts_with("/users/")
            || token.starts_with("/Volumes/")
            || token.starts_with("/System/Volumes/Data/Users/")
        {
            return true;
        }
        let p = Path::new(token);
        if !p.is_absolute() {
            return false;
        }
        if self
            .guard
            .home_variants()
            .iter()
            .any(|hv| path_starts_with_os(p, hv))
        {
            return true;
        }
        path_starts_with_os(p, &self.workspace.host_root)
    }
}

/// Splits a macOS Finder drag-and-drop or Cmd+V paste line into individual unescaped paths.
/// Rejects unmatched quotes or trailing unescaped backslash so malformed pastes fail closed.
pub fn split_finder_paste_tokens(input: &str) -> Result<Vec<String>, BerthError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            } else if c == '\\' {
                let Some(next) = chars.next() else {
                    return Err(BerthError::InvalidArgument(
                        "dangling backslash escape inside double-quoted host path".into(),
                    ));
                };
                cur.push(next);
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            '\\' => {
                has_token = true;
                let Some(next) = chars.next() else {
                    return Err(BerthError::InvalidArgument(
                        "dangling trailing backslash in host path paste".into(),
                    ));
                };
                cur.push(next);
            }
            w if w.is_ascii_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            other => {
                has_token = true;
                cur.push(other);
            }
        }
    }
    if in_single || in_double {
        return Err(BerthError::InvalidArgument(
            "unmatched quote in host path paste".into(),
        ));
    }
    if has_token {
        out.push(cur);
    }
    Ok(out)
}

pub fn shell_quote_if_needed(path: &str) -> String {
    if path.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')
    }) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Use exact platform ABI flags via `libc` (`O_NOFOLLOW | O_NONBLOCK` prevents both
        // symlink swap and FIFO blocking before `fstat()`).
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    opts.open(path)
}

#[cfg(unix)]
fn open_directory_nofollow_verified(
    canon: &Path,
    expected_id: FileIdentity,
) -> Result<File, BerthError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = OpenOptions::new();
    opts.read(true);
    opts.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let fd = opts.open(canon).map_err(|e| {
        BerthError::InvalidArgument(format!(
            "failed to pin authorized directory `{}`: {e}",
            canon.display()
        ))
    })?;
    let fmeta = fd.metadata()?;
    if !fmeta.is_dir() {
        return Err(BerthError::InvalidArgument(format!(
            "authorized target `{}` is no longer a directory",
            canon.display()
        )));
    }
    let open_id = FileIdentity::from_metadata(&fmeta);
    if open_id != expected_id {
        return Err(BerthError::InvalidArgument(format!(
            "directory `{}` changed during authorization (TOCTOU rejected)",
            canon.display()
        )));
    }
    Ok(fd)
}

fn parse_wsl_drvfs_path(p: &Path) -> Option<(char, PathBuf)> {
    let mut comps = p.components();
    if comps.next()? != Component::RootDir {
        return None;
    }
    if comps.next()?.as_os_str() != "mnt" {
        return None;
    }
    let drive_str = comps.next()?.as_os_str().to_str()?;
    if drive_str.len() != 1 {
        return None;
    }
    let ch = drive_str.chars().next()?;
    if !ch.is_ascii_alphabetic() {
        return None;
    }
    let mut rest = PathBuf::new();
    for c in comps {
        rest.push(c.as_os_str());
    }
    Some((ch.to_ascii_lowercase(), rest))
}

fn detect_active_wsl_windows_profile(real_home: &Path) -> Option<(char, String)> {
    // 1) Authoritative: host HOME itself resides on DrvFs `/mnt/<drive>/Users/<user>`
    if let Some((drive, rest)) = parse_wsl_drvfs_path(real_home) {
        let comps: Vec<_> = rest.components().collect();
        if comps.len() >= 2 && comps[0].as_os_str().eq_ignore_ascii_case("Users") {
            return Some((drive, comps[1].as_os_str().to_string_lossy().into_owned()));
        }
    }
    // 2) Authoritative Windows environment variable (`USERPROFILE=C:\Users\<user>`) when exposed.
    if let Ok(up) = std::env::var("USERPROFILE") {
        let trimmed = up.trim();
        let bytes = trimmed.as_bytes();
        if bytes.len() >= 4
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
        {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = trimmed[3..].replace('\\', "/");
            let mut parts = rest.split('/').filter(|s| !s.is_empty());
            if let (Some(first), Some(user)) = (parts.next(), parts.next()) {
                if first.eq_ignore_ascii_case("Users") && !user.is_empty() {
                    return Some((drive, user.to_string()));
                }
            }
        }
    }
    None
}

fn sanitize_alias_slug(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 28 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "dir".into()
    } else {
        trimmed
    }
}

fn sanitize_alias_filename(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches(|c| c == '.' || c == '_').to_string();
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed
    }
}

pub fn is_sensitive_filename(name: &str) -> bool {
    let lc = name.trim().to_ascii_lowercase();
    if lc == ".env"
        || lc.starts_with(".env.")
        || lc.ends_with(".pem")
        || lc.ends_with(".key")
        || lc.ends_with(".p12")
        || lc.ends_with(".pfx")
        || lc.ends_with(".jks")
        || lc.ends_with(".kdbx")
        || lc.starts_with("id_rsa")
        || lc.starts_with("id_ed25519")
        || lc.starts_with("id_ecdsa")
        || lc.starts_with("id_dsa")
        || matches!(
            lc.as_str(),
            ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".git-credentials"
                | "berth.json"
                | "auth.json"
                | "credentials.json"
                | "cookies.sqlite"
                | "login.keychain-db"
        )
    {
        return true;
    }
    lc.contains("secret") || lc.contains("credential") || lc.contains("private_key")
}

fn best_effort_canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn strip_macos_data_volume(p: &Path) -> Option<PathBuf> {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("/System/Volumes/Data/") {
        return Some(PathBuf::from(format!("/{rest}")));
    }
    if s == "/System/Volumes/Data" {
        return Some(PathBuf::from("/"));
    }
    None
}

fn path_eq_os(a: &Path, b: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(target_os = "macos"))]
    {
        a == b
    }
}

fn path_starts_with_os(path: &Path, base: &Path) -> bool {
    strip_prefix_os(path, base).is_ok()
}

fn strip_prefix_os(path: &Path, base: &Path) -> Result<PathBuf, ()> {
    #[cfg(target_os = "macos")]
    {
        let p_comps: Vec<_> = path.components().collect();
        let b_comps: Vec<_> = base.components().collect();
        if p_comps.len() < b_comps.len() {
            return Err(());
        }
        for (pc, bc) in p_comps.iter().zip(b_comps.iter()) {
            if !pc
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&bc.as_os_str().to_string_lossy())
            {
                return Err(());
            }
        }
        let mut out = PathBuf::new();
        for c in &p_comps[b_comps.len()..] {
            out.push(c.as_os_str());
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.strip_prefix(base).map(Path::to_path_buf).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let fake_home = tmp.path().join("home");
        let state = fake_home.join(".local/state/berth");
        let cfg = fake_home.join(".config/berth");
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&cfg).unwrap();
        fs::create_dir_all(fake_home.join(".ssh")).unwrap();
        fs::write(fake_home.join(".ssh/id_ed25519"), "SECRET-KEY").unwrap();
        (tmp, fake_home, state, cfg, staging)
    }

    #[test]
    fn workspace_plan_ascends_to_git_root_and_preserves_relative_cwd() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let repo = home.join("code/monorepo");
        let sub = repo.join("packages/web");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&sub).unwrap();

        let session = MountSession::new(&sub, &home, &state, &cfg, staging).unwrap();
        assert_eq!(session.workspace.host_root, fs::canonicalize(&repo).unwrap());
        assert_eq!(session.workspace.relative_cwd, PathBuf::from("packages/web"));
        assert_eq!(
            session.workspace.guest_cwd,
            PathBuf::from("/workspace/packages/web")
        );
        assert!(!session.workspace.has_external_gitdir);
        assert_eq!(session.workspace.workspace_id.len(), 12);
    }

    #[test]
    fn workspace_plan_detects_git_worktree_file_without_exporting_external_gitdir() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let wt = home.join("code/wt-feature");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /somewhere/else/.git/worktrees/wt").unwrap();

        let session = MountSession::new(&wt, &home, &state, &cfg, staging).unwrap();
        assert_eq!(session.workspace.host_root, fs::canonicalize(&wt).unwrap());
        assert!(session.workspace.has_external_gitdir);
    }

    #[test]
    fn denylist_blocks_home_ssh_state_symlinks_and_sensitive_basenames() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let proj = home.join("code/projA");
        let desktop = home.join("Desktop");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&desktop).unwrap();
        fs::write(desktop.join(".env.production"), "TOKEN=1").unwrap();
        fs::write(desktop.join("notes.txt"), "hello world").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(home.join(".ssh"), desktop.join("innocent-link")).unwrap();
        }

        let mut session = MountSession::new(&proj, &home, &state, &cfg, staging).unwrap();

        // Direct sensitive paths blocked.
        assert!(session.grant_directory(&home.to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.grant_directory(&home.join(".ssh").to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.grant_directory(&state.to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.grant_directory(&cfg.to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.resolve_or_grant_host_path(&home.join(".ssh/id_ed25519").to_string_lossy(), MountAccess::Ro, true).is_err());
        // Symlink to ~/.ssh blocked!
        #[cfg(unix)]
        assert!(session.grant_directory(&desktop.join("innocent-link").to_string_lossy(), MountAccess::Ro).is_err());
        // Sensitive filename blocked for external file attachment!
        assert!(session.resolve_or_grant_host_path(&desktop.join(".env.production").to_string_lossy(), MountAccess::Ro, true).is_err());
        // Traversal & URI syntax blocked!
        assert!(session.resolve_or_grant_host_path("~/Desktop/../.ssh/id_ed25519", MountAccess::Ro, true).is_err());
        assert!(session.resolve_or_grant_host_path("file:///etc/passwd", MountAccess::Ro, true).is_err());

        // Legitimate single file snapshots into `/mnt/ext/_files/` without exposing sibling `.env.production`!
        let snap = session
            .resolve_or_grant_host_path(&desktop.join("notes.txt").to_string_lossy(), MountAccess::Ro, true)
            .unwrap();
        assert!(snap.is_file_snapshot);
        assert!(!snap.needs_vz_update);
        assert!(snap.guest_path.starts_with("/mnt/ext/_files/f0001-"));
        assert!(session.live_dirs().is_empty(), "file snapshot must not expose parent Desktop dir");
    }

    #[test]
    fn live_directory_grant_rejects_ro_to_rw_upgrade_and_rewrites_finder_pastes() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let proj_a = home.join("code/projA");
        let proj_b = home.join("code/My Project B");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        fs::write(proj_a.join("main.rs"), "fn main(){}").unwrap();
        fs::write(proj_b.join("spec.md"), "# spec").unwrap();

        let mut session = MountSession::new(&proj_a, &home, &state, &cfg, staging).unwrap();

        // Grant proj_b RO -> succeeds and bumps generation 0 -> 1.
        let g1 = session.grant_directory(&proj_b.to_string_lossy(), MountAccess::Ro).unwrap();
        assert!(g1.needs_vz_update);
        assert_eq!(g1.base_generation, 0);
        assert_eq!(g1.next_generation, 1);
        assert!(g1.guest_path.starts_with("/mnt/ext/d0001-"));

        // Implicit RO -> RW upgrade on same dir is rejected!
        assert!(session.grant_directory(&proj_b.to_string_lossy(), MountAccess::Rw).is_err());

        // Finder escaped paste of workspace file maps cleanly to `/workspace/main.rs`!
        let escaped_a = proj_a.join("main.rs").to_string_lossy().replace(' ', "\\ ");
        match session.rewrite_host_paste(&escaped_a) {
            PasteRewriteOutcome::Rewritten { text, needs_vz_update, .. } => {
                assert_eq!(text, "/workspace/main.rs");
                assert!(!needs_vz_update);
            }
            other => panic!("expected Rewritten, got {other:?}"),
        }

        // Finder escaped paste of external directory with spaces maps to existing `/mnt/ext/d0001-...`!
        let escaped_b = proj_b.to_string_lossy().replace(' ', "\\ ");
        match session.rewrite_host_paste(&escaped_b) {
            PasteRewriteOutcome::Rewritten { text, .. } => {
                assert_eq!(text, g1.guest_path);
            }
            other => panic!("expected Rewritten, got {other:?}"),
        }

        // Pasting ~/.ssh/id_ed25519 is BLOCKED so raw host path never reaches guest!
        match session.rewrite_host_paste("~/.ssh/id_ed25519") {
            PasteRewriteOutcome::Blocked { .. } => {}
            other => panic!("expected Blocked, got {other:?}"),
        }

        // Ordinary prose paste passes through untouched!
        assert_eq!(
            session.rewrite_host_paste("hello world please fix the bug"),
            PasteRewriteOutcome::Unchanged
        );
    }

    #[test]
    fn startup_add_dir_rewrites_option_positions_only_before_double_dash() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let proj_a = home.join("code/projA");
        let lib_b = home.join("code/shared-lib");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&lib_b).unwrap();

        let mut session = MountSession::new(&proj_a, &home, &state, &cfg, staging).unwrap();
        let args = vec![
            "--add-dir".into(),
            lib_b.to_string_lossy().into_owned(),
            "-p".into(),
            "hello".into(),
            "--".into(),
            "--add-dir".into(),
            "/etc/passwd".into(),
        ];
        let rewritten = session.rewrite_startup_add_dirs("claude", &args).unwrap();
        assert_eq!(rewritten[0], "--add-dir");
        assert!(rewritten[1].starts_with("/mnt/ext/d0001-"));
        assert_eq!(rewritten[5], "--add-dir");
        assert_eq!(rewritten[6], "/etc/passwd");

        // Trailing `--add-dir` without a value fails closed immediately!
        assert!(
            session
                .rewrite_startup_add_dirs("codex", &["--add-dir".into()])
                .is_err()
        );
    }

    #[test]
    fn adversarial_security_atomicity_twophase_and_conflict_matrix() {
        let (tmp, home, state, cfg, staging) = make_fixture();
        let proj = home.join("code/projA");
        let ext_rw = home.join("code/ext_rw");
        let ext_ro = home.join("code/ext_ro");
        let child_of_rw = ext_rw.join("sub");
        let parent_dir = home.join("code/parent_holder");
        let child_dir = parent_dir.join("child");
        let desktop = home.join("Desktop");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&child_of_rw).unwrap();
        fs::create_dir_all(&ext_ro).unwrap();
        fs::create_dir_all(&child_dir).unwrap();
        fs::create_dir_all(&desktop).unwrap();
        fs::write(desktop.join("ok.txt"), "safe content").unwrap();
        fs::write(desktop.join("secret.env"), "SECRET=1").unwrap();
        fs::write(desktop.join(".env"), "KEY=1").unwrap();
        fs::write(desktop.join("single.txt"), "one").unwrap();

        #[cfg(unix)]
        {
            // Workspace symlink escaping to an otherwise-allowable external dir MUST fail (H2)!
            std::os::unix::fs::symlink(&ext_ro, proj.join("escape-link")).unwrap();
            // Hardlink (nlink == 2) MUST be rejected for single-file snapshot!
            fs::hard_link(desktop.join("single.txt"), desktop.join("hardlink.txt")).unwrap();
        }

        let mut session = MountSession::new(&proj, &home, &state, &cfg, staging).unwrap();

        // H1: RW -> RO exact match, child-of-RW RO, parent-of-RO RW, and RO inside writable /workspace MUST ALL FAIL!
        session
            .grant_directory(&ext_rw.to_string_lossy(), MountAccess::Rw)
            .unwrap();
        assert!(session.grant_directory(&ext_rw.to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.grant_directory(&child_of_rw.to_string_lossy(), MountAccess::Ro).is_err());
        assert!(session.grant_directory(&proj.to_string_lossy(), MountAccess::Ro).is_err());
        session
            .grant_directory(&child_dir.to_string_lossy(), MountAccess::Ro)
            .unwrap();
        assert!(session.grant_directory(&parent_dir.to_string_lossy(), MountAccess::Rw).is_err());

        // H1: RO grant for a PARENT of writable `/workspace` MUST be rejected!
        let parent_of_ws = proj.parent().unwrap();
        assert!(
            session
                .grant_directory(&parent_of_ws.to_string_lossy(), MountAccess::Ro)
                .is_err(),
            "RO grant on parent of writable /workspace must fail closed"
        );

        // H2: Workspace symlink escape MUST NOT silently create an external grant — including
        // multi-hop intermediate symlinks reached through an alternate symlinked parent prefix!
        #[cfg(unix)]
        {
            let before_dirs = session.live_dirs().len();
            assert!(
                session
                    .resolve_or_grant_host_path(
                        &proj.join("escape-link").to_string_lossy(),
                        MountAccess::Ro,
                        true
                    )
                    .is_err()
            );
            let ws_alias_parent = tmp.path().join("ws-alias-link");
            std::os::unix::fs::symlink(&proj, &ws_alias_parent).unwrap();
            let deep_escape_dir = proj.join("nested_link");
            std::os::unix::fs::symlink(&ext_ro, &deep_escape_dir).unwrap();
            assert!(
                session
                    .resolve_or_grant_host_path(
                        &ws_alias_parent.join("nested_link").join("ro.txt").to_string_lossy(),
                        MountAccess::Ro,
                        true
                    )
                    .is_err(),
                "multi-hop intermediate workspace symlink escape via alternate prefix must be rejected"
            );
            assert_eq!(session.live_dirs().len(), before_dirs);
            // Hardlink nlink == 2 rejected!
            assert!(
                session
                    .resolve_or_grant_host_path(
                        &desktop.join("hardlink.txt").to_string_lossy(),
                        MountAccess::Ro,
                        true
                    )
                    .is_err()
            );
        }

        // H4: Mixed file+dir paste MUST keep staged files invisible in `exported/` until 2PC commit,
        // and `abort_pending()` MUST delete them completely with ZERO exposure!
        let mixed_dir = home.join("mixed_ext_dir");
        fs::create_dir_all(&mixed_dir).unwrap();
        let mixed_paste = format!("{} {}", desktop.join("ok.txt").display(), mixed_dir.display());
        let mixed_gen_before = session.generation();
        match session.stage_host_paste(&mixed_paste) {
            PasteRewriteOutcome::Rewritten { needs_vz_update: true, .. } => {}
            other => panic!("expected Rewritten(needs_vz_update=true) for mixed batch, got {other:?}"),
        }
        assert_eq!(
            fs::read_dir(&session.staging_dir).unwrap().count(),
            0,
            "mixed batch file must NOT enter exported/ before 2PC commit!"
        );
        session.abort_pending();
        assert_eq!(session.generation(), mixed_gen_before);
        assert!(session.file_snapshots().is_empty());
        assert_eq!(fs::read_dir(&session.staging_dir).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(&session.staging_work_dir).unwrap().count(),
            0,
            "aborted mixed batch must clean staged work/ temp files"
        );

        // H4 + H5: Denied multi-file paste MUST be 100% atomic (zero files published in exported dir, zero gen drift)!
        let gen_before = session.generation();
        let bad_batch = format!(
            "{} {}",
            desktop.join("ok.txt").display(),
            desktop.join(".env").display()
        );
        match session.stage_host_paste(&bad_batch) {
            PasteRewriteOutcome::Blocked { .. } => {}
            other => panic!("expected Blocked for mixed valid+denied batch, got {other:?}"),
        }
        assert_eq!(session.generation(), gen_before);
        assert!(session.file_snapshots().is_empty());
        assert_eq!(
            fs::read_dir(&session.staging_dir).unwrap().count(),
            0,
            "exported staging dir must remain empty after denied batch"
        );

        // H7: Two-phase commit & abort keeps host generation synchronized with Swift!
        let staged = session
            .stage_attach(&ext_ro.to_string_lossy(), MountAccess::Ro, true)
            .unwrap();
        assert!(staged.needs_vz_update);
        assert_eq!(session.generation(), gen_before, "generation must not advance before commit");
        // Another attach while uncommitted fails closed!
        assert!(session.stage_attach(&ext_ro.to_string_lossy(), MountAccess::Ro, true).is_err());
        // Abort rolls back cleanly; re-staging succeeds and commit advances generation!
        session.abort_pending();
        assert_eq!(session.generation(), gen_before);
        let staged2 = session
            .stage_attach(&ext_ro.to_string_lossy(), MountAccess::Ro, true)
            .unwrap();
        assert_eq!(staged2.base_generation, gen_before);
        assert_eq!(staged2.next_generation, gen_before + 1);
        session.commit_pending(staged2.next_generation).unwrap();
        assert_eq!(session.generation(), gen_before + 1);

        // P2: Exact `/Users` paste and unmatched quotes fail closed (Blocked, never Unchanged!).
        match session.rewrite_host_paste("/Users") {
            PasteRewriteOutcome::Blocked { .. } => {}
            other => panic!("expected Blocked for /Users, got {other:?}"),
        }
        match session.rewrite_host_paste("'/Users/alice/unclosed") {
            PasteRewriteOutcome::Blocked { .. } => {}
            other => panic!("expected Blocked for unclosed quote, got {other:?}"),
        }

        // H9: WSL Windows profile policy blocks `/mnt/c/Users`, `/mnt/c/Users/bob`, and `AppData`/`.ssh`.
        let guard = SensitivePathGuard::new(&home, &state, &cfg, &[]);
        assert!(guard.is_dynamic_exact_denied_root(Path::new("/mnt/c/Users")));
        assert!(guard.is_dynamic_exact_denied_root(Path::new("/mnt/c/Users/bob")));
        assert!(guard.is_wsl_windows_sensitive_path(Path::new("/mnt/c/Users/bob/AppData/Roaming")));
        assert!(guard.is_wsl_windows_sensitive_path(Path::new("/mnt/c/Users/bob/.ssh/id_rsa")));
        assert!(!guard.is_dynamic_exact_denied_root(Path::new("/mnt/c/Users/bob/Projects/app")));
        assert!(!guard.is_wsl_windows_sensitive_path(Path::new("/mnt/c/Users/bob/Projects/app")));

        // H18: Distinct raw non-UTF-8 Unix workspace names hash to distinct workspace IDs!
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let p1 = Path::new(OsStr::from_bytes(b"/tmp/proj-\x80"));
            let p2 = Path::new(OsStr::from_bytes(b"/tmp/proj-\x81"));
            assert_ne!(compute_workspace_id(p1), compute_workspace_id(p2));
        }
    }
}
