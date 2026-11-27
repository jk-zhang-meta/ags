//! High-precision two-pass secret redaction for portable session bundles (`.bhv1`).
//!
//! Designed to protect uploaded sessions without corrupting ordinary code snippets,
//! Git SHAs, UUIDs, `data:image` URIs, or `${ENV_VAR}` placeholders:
//!
//! 1. **Pass 1 — Zero-FP Exact-Value Aho-Corasick**:
//!    Scrubs exact secrets already known to `bh` (Berth refresh/access tokens, `sk-` keys,
//!    CageShim proxy tokens, lease tokens, sensitive ambient environment variables).
//! 2. **Pass 2 — Anchored Structural Regex Rules**:
//!    Catches Anthropic/OpenAI/GitHub/AWS/Google/Slack/Stripe keys, structurally valid
//!    3-segment JWTs (decoded header contains `"alg"`), multiline PEM/OpenSSH private keys
//!    (both raw and JSON-escaped `\n`), URL userinfo passwords (`scheme://user:pass@host`),
//!    and `Authorization: Bearer/Basic` headers.
//! 3. **Deterministic JSON-Safe Replacement**:
//!    Replaces spans with `[REDACTED:<rule>:<hash8>]` (no quotes/backslashes -> preserves
//!    valid JSONL line-by-line!) and strips any Claude signed `thinking` block whose text
//!    was mutated so Anthropic's API never rejects a resumed redacted session.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use aho_corasick::AhoCorasick;
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REDACTION_RULESET_VERSION: &str = "bh-redact-v1";

/// Summary of redactions applied to a session bundle (never stores secret values!).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionReport {
    pub ruleset_version: String,
    pub total_redactions: usize,
    pub rules_hit: BTreeMap<String, usize>,
    pub dropped_signed_thinking_blocks: usize,
}

impl RedactionReport {
    pub fn new() -> Self {
        Self {
            ruleset_version: REDACTION_RULESET_VERSION.to_string(),
            total_redactions: 0,
            rules_hit: BTreeMap::new(),
            dropped_signed_thinking_blocks: 0,
        }
    }

    pub fn record(&mut self, rule: &str) {
        self.total_redactions += 1;
        *self.rules_hit.entry(rule.to_string()).or_insert(0) += 1;
    }

    pub fn merge(&mut self, other: &RedactionReport) {
        self.total_redactions += other.total_redactions;
        self.dropped_signed_thinking_blocks += other.dropped_signed_thinking_blocks;
        for (k, v) in &other.rules_hit {
            *self.rules_hit.entry(k.clone()).or_insert(0) += *v;
        }
    }
}

/// Configured redaction context carrying exact runtime secrets for Pass 1.
#[derive(Debug, Clone)]
pub struct Redactor {
    salt: String,
    exact_secrets: Vec<String>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new("berth-default-salt", Vec::new())
    }
}

struct PatternRule {
    name: &'static str,
    regex: Regex,
    /// When set, replace only the specified capture group instead of the entire match.
    secret_group: Option<usize>,
}

fn compiled_rules() -> &'static [PatternRule] {
    static RULES: OnceLock<Vec<PatternRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let defs: &[(&str, &str, Option<usize>)] = &[
            // Multiline PEM / OpenSSH private keys (raw or JSON-escaped \n).
            (
                "pem_private_key",
                r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED |PGP )?PRIVATE KEY-----(?:\\n|\r?\n|[\s\S])*?-----END (?:RSA |EC |DSA |OPENSSH |ENCRYPTED |PGP )?PRIVATE KEY-----",
                None,
            ),
            // Anthropic API & OAuth keys.
            (
                "anthropic_key",
                r"\bsk-ant-(?:api03|admin01|oat01|ort01)-[A-Za-z0-9_\-]{32,}\b",
                None,
            ),
            // OpenAI / Codex project, service-account, and admin keys.
            (
                "openai_key",
                r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_\-]{32,}\b",
                None,
            ),
            // Generic sk- keys (Berth sk-... / legacy OpenAI sk-...) >= 32 chars.
            ("sk_api_key", r"\bsk-[A-Za-z0-9_\-]{32,}\b", None),
            // GitHub PATs (classic & fine-grained).
            (
                "github_token",
                r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})\b",
                None,
            ),
            // AWS Access Key IDs.
            ("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", None),
            // Google API keys & OAuth tokens.
            (
                "google_key",
                r"\b(?:AIza[0-9A-Za-z_\-]{35}|ya29\.[0-9A-Za-z_\-]{20,}|1//[0-9A-Za-z_\-]{30,})\b",
                None,
            ),
            // Slack & Stripe live keys.
            (
                "slack_stripe_key",
                r"\b(?:xox[abposr]-[0-9A-Za-z\-]{20,}|sk_live_[0-9A-Za-z]{20,}|rk_live_[0-9A-Za-z]{20,})\b",
                None,
            ),
            // Berth local CageShim / ForwardProxy loopback credential URLs: `http://bh:<token>@127.0.0.1:...`
            (
                "proxy_userinfo_password",
                r"(?i)\b(?:https?|socks5h?)://([^:/@\s]+):([^@\s/]{8,})@(?:127\.0\.0\.1|localhost|\[::1\]|[A-Za-z0-9.\-]+)",
                Some(2),
            ),
            // Authorization: Bearer / Basic headers (value group only).
            (
                "auth_header",
                r#"(?i)\bAuthorization\s*[:=]\s*["']?(?:Bearer|Basic)\s+([A-Za-z0-9._\-+/=]{16,})"#,
                Some(1),
            ),
            // Berth lease header (`X-Berth-Lease: ...`).
            (
                "berth_lease",
                r#"(?i)\bX-Berth-Lease\s*[:=]\s*["']?([A-Za-z0-9._\-+/=]{20,})"#,
                Some(1),
            ),
            // AWS Secret Access Key in explicit assignment context.
            (
                "aws_secret_key",
                r#"(?i)\baws_secret_access_key\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})["']?"#,
                Some(1),
            ),
            // Structurally valid 3-segment JWT (validated in code below for `"alg"` header).
            (
                "jwt",
                r"\b(eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{12,})\b",
                Some(1),
            ),
        ];

        defs.iter()
            .map(|(name, pat, group)| PatternRule {
                name,
                regex: Regex::new(pat).expect("valid static redaction regex"),
                secret_group: *group,
            })
            .collect()
    })
}

/// Files that MUST NEVER be captured into any portable session bundle.
pub fn is_forbidden_capture_path(rel_path: &str) -> bool {
    let norm = rel_path.replace('\\', "/").to_ascii_lowercase();
    let file_name = norm.rsplit('/').next().unwrap_or(&norm);
    if matches!(
        file_name,
        "auth.json"
            | ".credentials.json"
            | ".claude.json"
            | "berth.json"
            | "config.toml"
            | "pool.json"
            | "identity.agekey"
    ) {
        return true;
    }
    if file_name.starts_with(".env")
        || file_name.starts_with("settings") && file_name.ends_with(".json")
        || file_name.ends_with(".key")
        || file_name.ends_with(".pem")
        || file_name.ends_with(".agekey")
    {
        return true;
    }
    norm.starts_with("session-env/")
        || norm.contains("/session-env/")
        || norm.starts_with(".ssh/")
        || norm.contains("/.ssh/")
}

impl Redactor {
    pub fn new(salt: impl Into<String>, exact_secrets: Vec<String>) -> Self {
        let filtered: Vec<String> = exact_secrets
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= 12)
            .collect();
        Self {
            salt: salt.into(),
            exact_secrets: filtered,
        }
    }

    /// Builds a redactor preloaded with known runtime secrets from Berth config & sensitive env vars.
    pub fn from_runtime_environment(berth_config: Option<&crate::berth::BerthConfig>) -> Self {
        let mut secrets = Vec::new();
        let mut salt = String::from("berth-local");

        if let Some(cfg) = berth_config {
            if let Some(ref session) = cfg.user_session {
                if !session.email.is_empty() {
                    salt = session.email.clone();
                }
                if !session.access_token.is_empty() {
                    secrets.push(session.access_token.clone());
                }
                if let Some(ref t) = session.refresh_token {
                    secrets.push(t.clone());
                }
            }
            if !cfg.api_key.is_empty() {
                secrets.push(cfg.api_key.clone());
            }
            for k in cfg.platform_keys.values() {
                secrets.push(k.clone());
            }
        }

        for (k, v) in std::env::vars() {
            let u = k.to_ascii_uppercase();
            if v.len() >= 16
                && (u.contains("TOKEN")
                    || u.contains("SECRET")
                    || u.contains("API_KEY")
                    || u.contains("PASSWORD")
                    || u.contains("PASSWD"))
            {
                secrets.push(v);
            }
        }

        secrets.sort();
        secrets.dedup();
        Self::new(salt, secrets)
    }

    fn placeholder(&self, rule: &str, secret: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.salt.as_bytes());
        h.update(b":");
        h.update(rule.as_bytes());
        h.update(b":");
        h.update(secret.as_bytes());
        let digest = h.finalize();
        let hex8 = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
        format!("[REDACTED:{rule}:{hex8}]")
    }

    /// Redacts a single text string (preserving valid JSON escaping because placeholders
    /// contain only `[A-Za-z0-9:_-[\]]`).
    pub fn redact_text(&self, input: &str, report: &mut RedactionReport) -> String {
        if input.is_empty() {
            return String::new();
        }
        let mut current = input.to_string();

        // Pass 1: Zero-FP exact runtime secret matching via Aho-Corasick.
        if !self.exact_secrets.is_empty() {
            if let Ok(ac) = AhoCorasick::builder()
                .match_kind(aho_corasick::MatchKind::LeftmostLongest)
                .build(&self.exact_secrets)
            {
                let mut out = String::with_capacity(current.len());
                let mut last = 0usize;
                for m in ac.find_iter(&current) {
                    out.push_str(&current[last..m.start()]);
                    let matched = &current[m.start()..m.end()];
                    out.push_str(&self.placeholder("known_secret", matched));
                    report.record("known_secret");
                    last = m.end();
                }
                if last > 0 {
                    out.push_str(&current[last..]);
                    current = out;
                }
            }
        }

        // Pass 2: Anchored structural regex patterns.
        for rule in compiled_rules() {
            let mut out = String::with_capacity(current.len());
            let mut last = 0usize;
            let mut changed = false;

            for caps in rule.regex.captures_iter(&current) {
                let target = match rule.secret_group {
                    Some(idx) => match caps.get(idx) {
                        Some(m) => m,
                        None => continue,
                    },
                    None => match caps.get(0) {
                        Some(m) => m,
                        None => continue,
                    },
                };
                let secret_str = target.as_str();

                // Avoid false positives on placeholders or already-redacted markers.
                if secret_str.starts_with("[REDACTED:")
                    || secret_str.starts_with("${")
                    || secret_str.starts_with("process.env.")
                {
                    continue;
                }
                // Extra structural validation for JWTs: decoded header JSON must contain `"alg"`.
                if rule.name == "jwt" && !is_structurally_valid_jwt(secret_str) {
                    continue;
                }

                out.push_str(&current[last..target.start()]);
                out.push_str(&self.placeholder(rule.name, secret_str));
                report.record(rule.name);
                last = target.end();
                changed = true;
            }

            if changed {
                out.push_str(&current[last..]);
                current = out;
            }
        }

        current
    }

    /// Redacts a JSONL transcript line-by-line, keeping untouched lines byte-identical
    /// and dropping any Claude signed `thinking` block whose content was modified so
    /// Anthropic's API will not reject the resumed session.
    /// Fails closed on invalid UTF-8 instead of silently passing unredacted bytes through.
    pub fn redact_jsonl_bytes(
        &self,
        provider: &str,
        raw: &[u8],
        report: &mut RedactionReport,
    ) -> anyhow::Result<Vec<u8>> {
        let text = std::str::from_utf8(raw).map_err(|e| {
            anyhow::anyhow!(
                "refusing to archive non-UTF-8 transcript while redaction is active ({e}); pass `--no-redact` only if intentionally archiving raw binary"
            )
        })?;
        let is_claude = provider == "claude-code" || provider == "claude";
        let mut out = String::with_capacity(text.len());

        for segment in text.split_inclusive('\n') {
            let (line, nl) = match segment.strip_suffix('\n') {
                Some(l) => (l, "\n"),
                None => (segment, ""),
            };
            let before = report.total_redactions;
            let redacted_line = self.redact_text(line, report);

            if is_claude && report.total_redactions > before {
                if let Some(sanitized) = strip_mutated_claude_signed_thinking(&redacted_line, report) {
                    out.push_str(&sanitized);
                    out.push_str(nl);
                    continue;
                }
            }
            out.push_str(&redacted_line);
            out.push_str(nl);
        }

        Ok(out.into_bytes())
    }
}

fn is_structurally_valid_jwt(token: &str) -> bool {
    let Some((header_b64, _)) = token.split_once('.') else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header_b64) else {
        return false;
    };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    val.as_object()
        .and_then(|o| o.get("alg"))
        .and_then(|v| v.as_str())
        .is_some()
}

fn strip_mutated_claude_signed_thinking(
    line: &str,
    report: &mut RedactionReport,
) -> Option<String> {
    let mut obj: serde_json::Value = serde_json::from_str(line).ok()?;
    let blocks = obj
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())?;

    let mut dropped = 0usize;
    blocks.retain(|b| {
        let is_thinking = b.get("type").and_then(|v| v.as_str()) == Some("thinking");
        let has_sig = b
            .get("signature")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let text_redacted = b
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("[REDACTED:"))
            .unwrap_or(false);
        if is_thinking && has_sig && text_redacted {
            dropped += 1;
            false
        } else {
            true
        }
    });
    if dropped > 0 {
        report.dropped_signed_thinking_blocks += dropped;
        serde_json::to_string(&obj).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_deterministically_without_corrupting_normal_code() {
        let redactor = Redactor::new("user-salt", vec!["my-exact-lease-secret-9999".into()]);
        let mut rep = RedactionReport::new();

        // Clean code must survive byte-for-byte.
        let clean = r#"{"type":"user","cwd":"/work/repo","sha":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","uuid":"019c3eae-94c3-7d73-9b2a-9edb18f1563b","code":"const token = process.env.API_KEY;"}"#;
        let out_clean = redactor.redact_text(clean, &mut rep);
        assert_eq!(out_clean, clean);
        assert_eq!(rep.total_redactions, 0);

        // Synthetic secrets across all major categories must be scrubbed cleanly while keeping valid JSON.
        let dirty = format!(
            r#"{{"line":"ant=sk-ant-api03-{} gh=ghp_{} aws=AKIAIOSFODNN7EXAMPLE proxy=http://bh:supersecret123@127.0.0.1:41002 lease=my-exact-lease-secret-9999"}}"#,
            "A".repeat(40),
            "B".repeat(36),
        );
        let out_dirty = redactor.redact_text(&dirty, &mut rep);
        assert!(rep.total_redactions >= 5);
        assert!(!out_dirty.contains("sk-ant-api03-"));
        assert!(!out_dirty.contains("ghp_"));
        assert!(!out_dirty.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!out_dirty.contains("supersecret123"));
        assert!(!out_dirty.contains("my-exact-lease-secret-9999"));
        assert!(serde_json::from_str::<serde_json::Value>(&out_dirty).is_ok());

        // Deterministic placeholder for identical secret.
        let mut rep2 = RedactionReport::new();
        assert_eq!(out_dirty, redactor.redact_text(&dirty, &mut rep2));
    }

    #[test]
    fn drops_mutated_claude_signed_thinking_block() {
        let redactor = Redactor::new("s", vec!["secret-inside-thinking-12345".into()]);
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"saw secret-inside-thinking-12345","signature":"sig123"},{"type":"text","text":"done"}]}}"#;
        let mut rep = RedactionReport::new();
        let out = redactor
            .redact_jsonl_bytes("claude-code", line.as_bytes(), &mut rep)
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = v["message"]["content"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(rep.dropped_signed_thinking_blocks, 1);
    }

    #[test]
    fn blocks_sensitive_capture_paths() {
        assert!(is_forbidden_capture_path("auth.json"));
        assert!(is_forbidden_capture_path(".credentials.json"));
        assert!(is_forbidden_capture_path("session-env/abc/env.sh"));
        assert!(is_forbidden_capture_path("settings.local.json"));
        assert!(!is_forbidden_capture_path("projects/-work-app/abc.jsonl"));
        assert!(!is_forbidden_capture_path("sessions/2026/11/26/rollout-123.jsonl"));
    }
}
