//! Portable `.bhv1` session bundle writer/reader with:
//! - Lossless redacted native artifacts (`bhv1/manifest.json` + `bhv1/native/<rel>`)
//! - Strict archive allowlist (regular files only, zero symlinks/devices/`..`/credentials)
//! - High-ratio `zstd` compression with bounded decompression against zip-bombs
//! - Optional pure-Rust `age` E2EE encryption (ChaCha20-Poly1305 STREAM, passphrase or X25519
//!   identity — wire-compatible with legacy `ags` `.age` files!)
//! - Post-decrypt manifest binding verification (`bundle_id`, `provider`, `session_id`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use age::secrecy::SecretString;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::redact::{RedactionReport, Redactor, is_forbidden_capture_path};

pub const BHV1_SCHEMA_VERSION: u32 = 1;
/// Hard cap on total uncompressed payload extracted from any untrusted remote bundle (512 MiB).
pub const MAX_BUNDLE_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
/// Hard cap on number of file entries inside a single bundle.
pub const MAX_BUNDLE_ENTRIES: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema_version: u32,
    pub bundle_id: String,
    pub logical_id: String,
    pub created_at: DateTime<Utc>,
    pub producer_version: String,
    pub producer_os: String,
    pub producer_arch: String,
    pub provider: String,
    pub source_session_id: String,
    pub origin: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub workspace_hint: String,
    #[serde(default)]
    pub git_repo: String,
    #[serde(default)]
    pub git_branch: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub message_count: usize,
    pub files: Vec<BundleFileEntry>,
    pub redaction: RedactionReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFileEntry {
    pub role: String,
    pub rel_path: String,
    pub size: u64,
    pub sha256: String,
}

/// Encryption options for `.bhv1` bundles.
#[derive(Debug, Clone)]
pub enum BundleEncryption {
    None,
    Passphrase(String),
    X25519Recipient(String),
}

/// Decryption options for `.bhv1` bundles.
#[derive(Debug, Clone)]
pub enum BundleDecryption {
    None,
    Passphrase(String),
    IdentityFile(PathBuf),
}

/// Input describing one native file to include in a `.bhv1` bundle.
#[derive(Debug, Clone)]
pub struct BundleInputFile {
    pub role: String,
    pub rel_path: String,
    pub bytes: Vec<u8>,
}

/// Output of creating a `.bhv1` bundle ready for upload to any of the 4 backends.
#[derive(Debug, Clone)]
pub struct BuiltBundle {
    pub manifest: BundleManifest,
    pub bytes: Vec<u8>,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    pub sha256: String,
    pub encoding: String,
    pub encrypted: bool,
    pub sealed_meta: Option<Vec<u8>>,
}

/// Decoded `.bhv1` bundle ready for local materialization or cross-agent conversion.
#[derive(Debug, Clone)]
pub struct UnpackedBundle {
    pub manifest: BundleManifest,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Builds a redacted, `zstd`-compressed, optionally `age`-encrypted `.bhv1` bundle.
pub fn build_bundle(
    mut manifest: BundleManifest,
    inputs: Vec<BundleInputFile>,
    redactor: Option<&Redactor>,
    encryption: &BundleEncryption,
    zstd_level: i32,
) -> Result<BuiltBundle> {
    if inputs.is_empty() {
        bail!("cannot build empty .bhv1 bundle");
    }
    // Reserve 1 tar slot for `bhv1/manifest.json` so build & unpack share identical limits.
    if inputs.len() > MAX_BUNDLE_ENTRIES.saturating_sub(1) {
        bail!(
            "bundle exceeds max payload entry count ({})",
            MAX_BUNDLE_ENTRIES.saturating_sub(1)
        );
    }

    let mut report = RedactionReport::new();
    let mut processed: Vec<(String, String, Vec<u8>)> = Vec::with_capacity(inputs.len());
    let mut raw_total: u64 = 0;

    for inp in inputs {
        let rel = normalize_rel_path(&inp.rel_path)?;
        if is_forbidden_capture_path(&rel) {
            bail!("refusing to archive credential/sensitive path `{rel}`");
        }
        let lower_rel = rel.to_ascii_lowercase();
        let clean_bytes = match redactor {
            Some(r) if lower_rel.ends_with(".jsonl.zst") => {
                // Bounded decompress -> redact JSONL (fail closed on invalid UTF-8) -> re-compress.
                let decoded = decompress_zstd_bounded(&inp.bytes, MAX_BUNDLE_UNCOMPRESSED_BYTES)
                    .with_context(|| format!("bounded zstd decode of `{rel}` before redaction"))?;
                let redacted = r.redact_jsonl_bytes(&manifest.provider, &decoded, &mut report)?;
                zstd::bulk::compress(&redacted, zstd_level.clamp(1, 19))
                    .with_context(|| format!("re-compress redacted `{rel}`"))?
            }
            Some(r) if lower_rel.ends_with(".jsonl") || lower_rel.ends_with(".json") => {
                r.redact_jsonl_bytes(&manifest.provider, &inp.bytes, &mut report)?
            }
            Some(r) => match std::str::from_utf8(&inp.bytes) {
                Ok(t) => r.redact_text(t, &mut report).into_bytes(),
                Err(e) => {
                    bail!(
                        "refusing to archive undecodable binary/non-UTF-8 file `{rel}` while redaction is enabled ({e}); pass `--no-redact` explicitly if intended"
                    );
                }
            },
            None => inp.bytes,
        };
        raw_total = raw_total.saturating_add(clean_bytes.len() as u64);
        if raw_total > MAX_BUNDLE_UNCOMPRESSED_BYTES {
            bail!(
                "bundle payload size exceeds uncompressed cap ({MAX_BUNDLE_UNCOMPRESSED_BYTES} bytes)"
            );
        }
        processed.push((inp.role, rel, clean_bytes));
    }

    // Also scrub title/description/workspace/git metadata when redaction is active!
    if let Some(r) = redactor {
        manifest.title = r.redact_text(&manifest.title, &mut report);
        manifest.description = r.redact_text(&manifest.description, &mut report);
        manifest.git_repo = r.redact_text(&manifest.git_repo, &mut report);
    }

    if manifest.bundle_id.is_empty() {
        manifest.bundle_id = Uuid::now_v7().to_string();
    }
    manifest.schema_version = BHV1_SCHEMA_VERSION;
    manifest.redaction = report;
    manifest.files = processed
        .iter()
        .map(|(role, rel, data)| BundleFileEntry {
            role: role.clone(),
            rel_path: rel.clone(),
            size: data.len() as u64,
            sha256: sha256_hex(data),
        })
        .collect();

    // Build deterministic tar archive: `bhv1/manifest.json` first, then `bhv1/native/<rel>`.
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut tar_buf = Vec::with_capacity(manifest_bytes.len() + raw_total as usize + 4096);
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        append_tar_file(&mut builder, "bhv1/manifest.json", &manifest_bytes)?;
        for (_, rel, data) in &processed {
            let entry_path = format!("bhv1/native/{rel}");
            append_tar_file(&mut builder, &entry_path, data)?;
        }
        builder.finish()?;
    }
    if tar_buf.len() as u64 > MAX_BUNDLE_UNCOMPRESSED_BYTES {
        bail!(
            "uncompressed tar archive ({} bytes) exceeds cap ({MAX_BUNDLE_UNCOMPRESSED_BYTES} bytes)",
            tar_buf.len()
        );
    }

    // Compress with zstd (default level 9: fast & very high ratio on JSONL transcripts).
    let compressed = zstd::bulk::compress(&tar_buf, zstd_level.clamp(1, 19))
        .context("zstd compress .bhv1 bundle")?;

    let (final_bytes, encrypted, sealed_meta) = match encryption {
        BundleEncryption::None => (compressed, false, None),
        BundleEncryption::Passphrase(pass) => {
            let enc = age_encrypt_passphrase(&compressed, pass)?;
            let meta_enc = age_encrypt_passphrase(&manifest_bytes, pass)?;
            (enc, true, Some(meta_enc))
        }
        BundleEncryption::X25519Recipient(pubkey) => {
            let enc = age_encrypt_recipient(&compressed, pubkey)?;
            let meta_enc = age_encrypt_recipient(&manifest_bytes, pubkey)?;
            (enc, true, Some(meta_enc))
        }
    };

    let stored_bytes = final_bytes.len() as u64;
    let sha256 = sha256_hex(&final_bytes);

    Ok(BuiltBundle {
        manifest,
        bytes: final_bytes,
        raw_bytes: raw_total,
        stored_bytes,
        sha256,
        encoding: "zstd".to_string(),
        encrypted,
        sealed_meta,
    })
}

/// Verifies SHA-256 (when provided), decrypts (when `age`-armored/binary), decompresses
/// with strict anti-zip-bomb caps, validates every file hash and path allowlist, and checks
/// inner manifest binding (`expected_logical_id` / `expected_provider`).
pub fn unpack_bundle(
    raw: &[u8],
    expected_sha256: Option<&str>,
    decryption: &BundleDecryption,
    expected_logical_id: Option<&str>,
) -> Result<UnpackedBundle> {
    if let Some(exp) = expected_sha256 {
        let actual = sha256_hex(raw);
        if actual != exp.to_ascii_lowercase() {
            bail!("bundle SHA-256 mismatch: expected {exp}, got {actual}");
        }
    }

    let maybe_decrypted = if is_age_ciphertext(raw) {
        age_decrypt(raw, decryption)?
    } else {
        raw.to_vec()
    };

    // Detect zstd vs gzip (for backwards compatibility with older archives).
    let tar_bytes = if is_zstd_frame(&maybe_decrypted) {
        decompress_zstd_bounded(&maybe_decrypted, MAX_BUNDLE_UNCOMPRESSED_BYTES)?
    } else if is_gzip_frame(&maybe_decrypted) {
        decompress_gzip_bounded(&maybe_decrypted, MAX_BUNDLE_UNCOMPRESSED_BYTES)?
    } else {
        bail!("unrecognized bundle compression header (expected zstd or gzip)");
    };

    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let mut manifest: Option<BundleManifest> = None;
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut entry_count = 0usize;
    let mut total_extracted = 0u64;

    for entry_res in archive.entries()? {
        entry_count += 1;
        if entry_count > MAX_BUNDLE_ENTRIES {
            bail!("bundle exceeds maximum allowed entry count ({MAX_BUNDLE_ENTRIES})");
        }
        let mut entry = entry_res?;
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            continue;
        }
        if !etype.is_file() {
            bail!("refusing non-regular tar entry type {:?} in bundle", etype);
        }
        let raw_path = entry.path()?.to_string_lossy().replace('\\', "/");
        let size = entry.size();
        total_extracted = total_extracted.saturating_add(size);
        if total_extracted > MAX_BUNDLE_UNCOMPRESSED_BYTES {
            bail!("bundle extracted size exceeds cap ({MAX_BUNDLE_UNCOMPRESSED_BYTES} bytes)");
        }

        let mut buf = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut buf)?;

        if raw_path == "bhv1/manifest.json" {
            let parsed: BundleManifest =
                serde_json::from_slice(&buf).context("parse bhv1/manifest.json")?;
            if parsed.schema_version != BHV1_SCHEMA_VERSION {
                bail!("unsupported .bhv1 schema_version {}", parsed.schema_version);
            }
            manifest = Some(parsed);
        } else if let Some(rel) = raw_path.strip_prefix("bhv1/native/") {
            let norm = normalize_rel_path(rel)?;
            if is_forbidden_capture_path(&norm) {
                bail!("refusing sensitive path `{norm}` inside downloaded bundle");
            }
            files.insert(norm, buf);
        } else {
            bail!("unexpected path `{raw_path}` outside bhv1/ namespace");
        }
    }

    let manifest = manifest.context("missing bhv1/manifest.json in bundle")?;

    // Bind check: prevent server/remote swap of one session blob for another.
    if let Some(exp_logical) = expected_logical_id {
        if manifest.logical_id != exp_logical {
            bail!(
                "manifest logical_id mismatch (possible blob swap): expected `{exp_logical}`, got `{}`",
                manifest.logical_id
            );
        }
    }

    // Verify every file entry's size & SHA-256 against the inner manifest.
    if manifest.files.len() != files.len() {
        bail!(
            "manifest file count ({}) does not match archive payload ({})",
            manifest.files.len(),
            files.len()
        );
    }
    for entry in &manifest.files {
        let Some(data) = files.get(&entry.rel_path) else {
            bail!("manifest file `{}` missing from archive", entry.rel_path);
        };
        if data.len() as u64 != entry.size {
            bail!("size mismatch for `{}`", entry.rel_path);
        }
        if sha256_hex(data) != entry.sha256 {
            bail!("SHA-256 mismatch for `{}`", entry.rel_path);
        }
    }

    Ok(UnpackedBundle { manifest, files })
}

/// Ensures or creates a managed pure-Rust `age` X25519 identity at `identity_path` (`0600`).
/// Returns `(recipient_pubkey, identity_path)`.
pub fn ensure_age_identity(identity_path: &Path) -> Result<String> {
    if identity_path.is_file() {
        let content = fs::read_to_string(identity_path)?;
        return parse_age_recipient_from_identity(&content);
    }
    if let Some(parent) = identity_path.parent() {
        fs::create_dir_all(parent)?;
    }
    use age::secrecy::ExposeSecret;
    let id = age::x25519::Identity::generate();
    let recipient = id.to_public().to_string();
    let secret = id.to_string();
    let text = format!(
        "# Berth managed age identity (keep private, back up safely)\n# public key: {recipient}\n{}\n",
        secret.expose_secret()
    );
    crate::berth::write_private_file_atomic(identity_path, text.as_bytes())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(recipient)
}

fn parse_age_recipient_from_identity(content: &str) -> Result<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let id: age::x25519::Identity = trimmed
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid age X25519 identity: {e}"))?;
        return Ok(id.to_public().to_string());
    }
    bail!("no age X25519 identity found in file")
}

pub fn age_encrypt_passphrase(plain: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let encryptor =
        age::Encryptor::with_user_passphrase(SecretString::from(passphrase.to_string()));
    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out)?;
    writer.write_all(plain)?;
    writer.finish()?;
    Ok(out)
}

pub fn age_encrypt_recipient(plain: &[u8], pubkey: &str) -> Result<Vec<u8>> {
    let recipient: age::x25519::Recipient = pubkey
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid age X25519 recipient: {e}"))?;
    let recipients: Vec<Box<dyn age::Recipient + Send>> = vec![Box::new(recipient)];
    let encryptor = age::Encryptor::with_recipients(recipients.iter().map(|r| r.as_ref() as &dyn age::Recipient))
        .map_err(|e| anyhow::anyhow!("age encryptor init: {e}"))?;
    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out)?;
    writer.write_all(plain)?;
    writer.finish()?;
    Ok(out)
}

pub fn age_decrypt(ciphertext: &[u8], dec: &BundleDecryption) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| anyhow::anyhow!("invalid age header: {e}"))?;
    let mut reader = match dec {
        BundleDecryption::Passphrase(pass) => {
            let id = age::scrypt::Identity::new(SecretString::from(pass.clone()));
            decryptor
                .decrypt(std::iter::once(&id as &dyn age::Identity))
                .map_err(|e| anyhow::anyhow!("age passphrase decrypt failed: {e}"))?
        }
        BundleDecryption::IdentityFile(path) => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("read age identity file {}", path.display()))?;
            let ids = age::IdentityFile::from_buffer(Cursor::new(content))
                .context("parse age identity file")?
                .into_identities()?;
            decryptor
                .decrypt(ids.iter().map(|i| i.as_ref() as &dyn age::Identity))
                .map_err(|e| anyhow::anyhow!("age identity decrypt failed: {e}"))?
        }
        BundleDecryption::None => {
            bail!("bundle is age-encrypted; pass `--passphrase` or configure an age identity key");
        }
    };
    let mut out = Vec::new();
    reader
        .by_ref()
        .take(MAX_BUNDLE_UNCOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > MAX_BUNDLE_UNCOMPRESSED_BYTES {
        bail!("decrypted bundle exceeds safety cap");
    }
    Ok(out)
}

fn append_tar_file<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, path, Cursor::new(data))?;
    Ok(())
}

pub fn decompress_zstd_bounded(data: &[u8], max_bytes: u64) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(data))?;
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_bytes {
        bail!("zstd decompression exceeded safety cap ({max_bytes} bytes)");
    }
    Ok(out)
}

fn decompress_gzip_bounded(data: &[u8], max_bytes: u64) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(Cursor::new(data));
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_bytes {
        bail!("gzip decompression exceeded safety cap ({max_bytes} bytes)");
    }
    Ok(out)
}

fn is_age_ciphertext(data: &[u8]) -> bool {
    data.starts_with(b"age-encryption.org/v1\n") || data.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----")
}

fn is_zstd_frame(data: &[u8]) -> bool {
    data.len() >= 4 && data[0..4] == [0x28, 0xB5, 0x2F, 0xFD]
}

fn is_gzip_frame(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B
}

fn normalize_rel_path(raw: &str) -> Result<String> {
    let replaced = raw.replace('\\', "/");
    let p = Path::new(&replaced);
    let mut parts = Vec::new();
    for comp in p.components() {
        match comp {
            Component::Normal(s) => {
                let st = s.to_string_lossy();
                if st.contains('\0') {
                    bail!("NUL byte in bundle path");
                }
                parts.push(st.into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("illegal non-relative path `{raw}` in bundle");
            }
        }
    }
    if parts.is_empty() {
        bail!("empty relative path in bundle");
    }
    Ok(parts.join("/"))
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> BundleManifest {
        BundleManifest {
            schema_version: BHV1_SCHEMA_VERSION,
            bundle_id: String::new(),
            logical_id: "codex:sess-42".into(),
            created_at: Utc::now(),
            producer_version: "2.3.0".into(),
            producer_os: std::env::consts::OS.into(),
            producer_arch: std::env::consts::ARCH.into(),
            provider: "codex".into(),
            source_session_id: "sess-42".into(),
            origin: "cage:acct-7".into(),
            title: "Fix vault auth".into(),
            description: "Important checkpoint".into(),
            workspace_hint: "/work/repo".into(),
            git_repo: "github.com/org/repo".into(),
            git_branch: "main".into(),
            tags: vec!["important".into()],
            message_count: 4,
            files: vec![],
            redaction: RedactionReport::new(),
        }
    }

    #[test]
    fn roundtrip_redacted_zstd_and_age_e2ee_bundle() {
        let redactor = Redactor::new("salt", vec!["super-secret-berth-lease-token".into()]);
        let payload = br#"{"type":"session_meta","payload":{"id":"sess-42","note":"super-secret-berth-lease-token"}}"#;

        // 1. Plain zstd bundle with redaction
        let built = build_bundle(
            sample_manifest(),
            vec![BundleInputFile {
                role: "transcript".into(),
                rel_path: "sessions/2026/11/26/rollout-sess-42.jsonl".into(),
                bytes: payload.to_vec(),
            }],
            Some(&redactor),
            &BundleEncryption::None,
            6,
        )
        .unwrap();
        assert!(!built.encrypted);
        assert_eq!(built.manifest.redaction.total_redactions, 1);

        let unpacked = unpack_bundle(
            &built.bytes,
            Some(&built.sha256),
            &BundleDecryption::None,
            Some("codex:sess-42"),
        )
        .unwrap();
        let text = String::from_utf8(
            unpacked.files["sessions/2026/11/26/rollout-sess-42.jsonl"].clone(),
        )
        .unwrap();
        assert!(!text.contains("super-secret-berth-lease-token"));
        assert!(text.contains("[REDACTED:known_secret:"));

        // Swap detection must reject wrong logical_id.
        assert!(
            unpack_bundle(
                &built.bytes,
                Some(&built.sha256),
                &BundleDecryption::None,
                Some("codex:other-session"),
            )
            .is_err()
        );

        // 2. Age passphrase E2EE roundtrip
        let enc_built = build_bundle(
            sample_manifest(),
            vec![BundleInputFile {
                role: "transcript".into(),
                rel_path: "sessions/2026/11/26/rollout-sess-42.jsonl".into(),
                bytes: payload.to_vec(),
            }],
            Some(&redactor),
            &BundleEncryption::Passphrase("test-passphrase-123".into()),
            6,
        )
        .unwrap();
        assert!(enc_built.encrypted);
        assert!(enc_built.sealed_meta.is_some());
        assert!(
            unpack_bundle(
                &enc_built.bytes,
                Some(&enc_built.sha256),
                &BundleDecryption::Passphrase("wrong".into()),
                Some("codex:sess-42"),
            )
            .is_err()
        );
        let dec = unpack_bundle(
            &enc_built.bytes,
            Some(&enc_built.sha256),
            &BundleDecryption::Passphrase("test-passphrase-123".into()),
            Some("codex:sess-42"),
        )
        .unwrap();
        assert_eq!(dec.manifest.source_session_id, "sess-42");
    }
}
