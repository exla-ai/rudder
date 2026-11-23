//! Text Rudder writes to its activity log is a short preview with secrets and
//! email addresses taken out. Users paste tokens into prompts, and the log is
//! synced to the phone and the app, so it must never carry them.

/// Longest activity line kept, in characters.
pub(crate) const ACTIVITY_TEXT_CHARS: usize = 240;

/// Well-known token prefixes and how many token characters must follow one
/// for the word to count as a token. The prefix is kept so the line still
/// says what kind of credential was there.
const TOKEN_PREFIXES: &[(&str, usize)] = &[
    // Vercel
    ("vcp_", 16), ("vci_", 16), ("vca_", 16), ("vcr_", 16), ("vck_", 16),
    // GitHub
    ("ghp_", 20), ("gho_", 20), ("ghu_", 20), ("ghs_", 20), ("ghr_", 20), ("github_pat_", 20),
    // GitLab, npm
    ("glpat-", 16), ("npm_", 20),
    // Anthropic (sk-ant-), OpenAI (sk-proj-) and other sk- keys
    ("sk-", 16),
    // AWS access key ids
    ("AKIA", 16), ("ASIA", 16),
    // Google API keys, OAuth access tokens and client secrets
    ("AIza", 30), ("ya29.", 20), ("GOCSPX-", 16),
    // Stripe
    ("sk_live_", 16), ("sk_test_", 16), ("rk_live_", 16), ("rk_test_", 16), ("whsec_", 16),
    // Slack
    ("xoxb-", 16), ("xoxp-", 16), ("xoxa-", 16), ("xoxs-", 16), ("xapp-", 16),
    // ComputeSDK, PostHog
    ("csdk_", 16), ("phx_", 16), ("phc_", 16),
];

const REDACTED: &str = "[redacted]";

/// `text` as the activity log stores it: secrets and email addresses
/// replaced, then cut to `ACTIVITY_TEXT_CHARS`.
pub(crate) fn activity_text(text: &str) -> String {
    let text = redact_secrets(text);
    if text.chars().count() <= ACTIVITY_TEXT_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(ACTIVITY_TEXT_CHARS - 1).collect();
    cut.push('\u{2026}');
    cut
}

/// Replace token-shaped words and email addresses in `text`.
pub(crate) fn redact_secrets(text: &str) -> String {
    let text = redact_emails(text);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut prev_word, mut prev_sep) = ("", "");
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if !is_word_byte(bytes[i]) {
            while i < bytes.len() && !is_word_byte(bytes[i]) {
                i += 1;
            }
            prev_sep = &text[start..i];
            out.push_str(prev_sep);
            continue;
        }
        while i < bytes.len() && is_word_byte(bytes[i]) {
            i += 1;
        }
        let word = &text[start..i];
        let assigned = prev_sep == "=" && is_secret_key(prev_word) && word.len() >= 8;
        let bearer = prev_word.eq_ignore_ascii_case("bearer") && prev_sep == " " && word.len() >= 16;
        if let Some(prefix) = token_prefix(word) {
            out.push_str(prefix);
            out.push_str(REDACTED);
        } else if assigned || bearer || is_jwt(word) || is_long_random(word) {
            out.push_str(REDACTED);
        } else {
            out.push_str(word);
        }
        prev_word = word;
    }
    out
}

/// Token characters, base64 included. `=` and `:` separate a key from its value.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+' | b'/')
}

fn token_prefix(word: &str) -> Option<&'static str> {
    TOKEN_PREFIXES
        .iter()
        .find(|(prefix, min)| word.starts_with(prefix) && word.len() - prefix.len() >= *min)
        .map(|(prefix, _)| *prefix)
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "api_key", "apikey", "api-key", "_key", "auth"]
        .iter()
        .any(|needle| key.contains(needle))
}

fn is_jwt(word: &str) -> bool {
    word.starts_with("eyJ") && word.len() >= 32 && word.contains('.')
}

/// A long random string: mixed case and digits, no dots, not a path. Run ids,
/// change ids, hashes and paths are lowercase or start with `/`, so they stay.
fn is_long_random(word: &str) -> bool {
    word.len() >= 32
        && !word.starts_with('/')
        && !word.contains('.')
        && word.bytes().any(|b| b.is_ascii_uppercase())
        && word.bytes().any(|b| b.is_ascii_lowercase())
        && word.bytes().any(|b| b.is_ascii_digit())
}

fn redact_emails(text: &str) -> String {
    let bytes = text.as_bytes();
    let local = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-');
    let domain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices('@') {
        if at < copied {
            continue;
        }
        let mut start = at;
        while start > copied && local(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < bytes.len() && domain(bytes[end]) {
            end += 1;
        }
        let host = text[at + 1..end].trim_end_matches('.');
        let end = at + 1 + host.len();
        let tld = host.rsplit_once('.').map(|(_, tld)| tld).unwrap_or("");
        if start == at || tld.len() < 2 || !tld.bytes().all(|b| b.is_ascii_alphabetic()) {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str("[email]");
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_text_redacts_token_shapes_and_emails_and_keeps_ids() {
        // Built at runtime so no token-shaped literal sits in the source.
        let body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6";
        for prefix in ["vcp_", "ghp_", "github_pat_", "npm_", "sk-ant-api03-", "sk-proj-", "AIza", "sk_live_", "csdk_", "xoxb-"] {
            let token = format!("{prefix}{body}");
            let line = activity_text(&format!("steered run-1: \"use {token} for deploys\""));
            assert!(!line.contains(body), "{prefix}: {line}");
            assert!(line.contains("[redacted]"), "{prefix}: {line}");
        }
        let aws = format!("AKIA{}", "ABCDEFGH23456789");
        assert_eq!(redact_secrets(&format!("key {aws}")), "key AKIA[redacted]");
        assert_eq!(redact_secrets(&format!("export VERCEL_TOKEN={body}")), "export VERCEL_TOKEN=[redacted]");
        assert_eq!(redact_secrets(&format!("Authorization: Bearer {body}")), "Authorization: Bearer [redacted]");
        assert_eq!(redact_secrets(&format!("raw {body} here")), "raw [redacted] here");
        assert_eq!(
            redact_secrets("mail someone.else+tag@example.co.uk, or mailto:a@b.io."),
            "mail [email], or mailto:[email]."
        );
        // Ids, hashes, paths and plain words are not secrets.
        let plain = "merged 1792643927097049865-build-iphone-lifting-tracker-with-leaderboard-26730 at \
            /workspace/code/Exla2Rudder/.rudder-workspaces/ConfigFile2026BuildsAgainstMain op \
            de4609ec5b1f0c2a9e8d7c6b5a4f3e2d1c0b9a8f (sk-learn, user@host, v5.0.33)";
        assert_eq!(redact_secrets(plain), plain);
        let long = "x".repeat(ACTIVITY_TEXT_CHARS * 2);
        let cut = activity_text(&long);
        assert_eq!(cut.chars().count(), ACTIVITY_TEXT_CHARS);
        assert_eq!(activity_text(&cut), cut, "an activity line is already a preview");
    }
}
