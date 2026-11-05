//! One place that knows what a secret looks like.
//!
//! Text leaves Reliaburger in several ways that a stranger may end up
//! reading: audit records, `relish agent` answers an LLM keeps in its
//! transcript, and (later in 0.2.7) a bug report a human pastes into a
//! GitHub issue. Each of those runs the same rules from here, so a rule added
//! once protects all of them.
//!
//! The rules are deterministic and deliberately narrow: each one recognises
//! a shape Reliaburger itself produces or handles (an API token, an
//! encrypted env value, an age identity, a PEM private key, a URL carrying
//! credentials). They're a second line of defence, not a guarantee: a
//! password typed into a log line in plain words still gets through.
//!
//! Every rule leaves a marker saying what was there, and running the rules
//! again over their own output changes nothing and counts nothing.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// What replaces a secret, inside whatever prefix the rule keeps.
pub const REDACTED: &str = "[redacted]";

/// One redaction rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// A Reliaburger API or join token, `rbrg_…`.
    ReliaburgerToken,
    /// An encrypted env value, `ENC[AGE:…]`; the prefix stays.
    EncryptedValue,
    /// An age identity, `AGE-SECRET-KEY-1…`.
    AgeIdentity,
    /// A PEM block whose label contains `PRIVATE KEY`.
    PemPrivateKey,
    /// The user information in a URL, `scheme://user:pass@host`.
    UrlCredentials,
    /// The value of an environment variable.
    EnvValue,
}

/// How many times each rule fired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionCounts {
    fired: BTreeMap<Rule, usize>,
}

impl RedactionCounts {
    /// How many times `rule` fired.
    pub fn count(&self, rule: Rule) -> usize {
        self.fired.get(&rule).copied().unwrap_or(0)
    }

    /// How many redactions in all.
    pub fn total(&self) -> usize {
        self.fired.values().sum()
    }

    /// Whether nothing was redacted.
    pub fn is_empty(&self) -> bool {
        self.fired.is_empty()
    }

    /// Each rule that fired, with its count, in a fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (Rule, usize)> + '_ {
        self.fired.iter().map(|(rule, count)| (*rule, *count))
    }

    fn record(&mut self, rule: Rule) {
        *self.fired.entry(rule).or_insert(0) += 1;
    }
}

/// `text` with every secret the rules recognise replaced by a marker,
/// counting each replacement in `counts`.
///
/// The rules run in a fixed order, widest first: a PEM block can hold
/// anything, so it goes before the rules that look inside lines, and the
/// token scanner goes last because every other rule may already have
/// swallowed a token.
pub fn redact(text: &str, counts: &mut RedactionCounts) -> String {
    let text = redact_pem_private_keys(text, counts);
    let text = redact_encrypted_values(&text, counts);
    let text = redact_age_identities(&text, counts);
    let text = redact_url_credentials(&text, counts);
    redact_reliaburger_tokens(&text, counts)
}

/// `text` with every secret the rules recognise replaced by a marker.
pub fn redact_secrets(text: &str) -> String {
    redact(text, &mut RedactionCounts::default())
}

/// An environment's keys, each with its value replaced by [`REDACTED`].
///
/// Env values are where secrets live, decrypted or not, and no pattern can
/// tell a password from a feature flag, so every value goes.
pub fn redact_env<K, V>(
    env: impl IntoIterator<Item = (K, V)>,
    counts: &mut RedactionCounts,
) -> BTreeMap<String, String>
where
    K: AsRef<str>,
{
    env.into_iter()
        .map(|(key, _)| {
            counts.record(Rule::EnvValue);
            (key.as_ref().to_string(), REDACTED.to_string())
        })
        .collect()
}

/// Compile one of this module's patterns.
///
/// Each pattern is a literal checked by every test in this module, so a
/// typo fails the build's tests, never a running node; the `expect` can't
/// fire in a binary that passed them.
fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the redaction patterns are valid regexes")
}

/// A PEM label naming a private key: `PRIVATE KEY`, `RSA PRIVATE KEY`,
/// `ENCRYPTED PRIVATE KEY` and so on.
const PRIVATE_KEY_LABEL: &str = "[A-Z0-9 ]*PRIVATE KEY[A-Z0-9 ]*";

/// A whole private key block, or one whose `END` never came (a log cut off
/// halfway, a truncated paste), which runs to the end of the text.
static PEM_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    pattern(&format!(
        r"(?s)-----BEGIN ({PRIVATE_KEY_LABEL})-----(.*?)(?:-----END ({PRIVATE_KEY_LABEL})-----|\z)"
    ))
});

/// The body lines of a key whose `BEGIN` fell before the text started: a
/// run of base64 lines straight before an `END`.
static PEM_ORPHANED_END: LazyLock<Regex> = LazyLock::new(|| {
    pattern(&format!(
        r"(?m)(?:^[A-Za-z0-9+/=]{{16,}}\r?\n)+-----END ({PRIVATE_KEY_LABEL})-----"
    ))
});

fn redact_pem_private_keys(text: &str, counts: &mut RedactionCounts) -> String {
    let text = PEM_BLOCK.replace_all(text, |captures: &Captures| {
        let begin = &captures[1];
        if &captures[2] == REDACTED {
            return captures[0].to_string();
        }
        counts.record(Rule::PemPrivateKey);
        match captures.get(3) {
            Some(end) => format!(
                "-----BEGIN {begin}-----{REDACTED}-----END {}-----",
                end.as_str()
            ),
            None => format!("-----BEGIN {begin}-----{REDACTED}"),
        }
    });
    PEM_ORPHANED_END
        .replace_all(&text, |captures: &Captures| {
            counts.record(Rule::PemPrivateKey);
            format!("{REDACTED}-----END {}-----", &captures[1])
        })
        .into_owned()
}

/// `ENC[AGE:…]`, up to its closing bracket, or to the first space or quote
/// if the bracket never comes. The first alternative is a value this module
/// already redacted.
static ENCRYPTED_VALUE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"ENC\[AGE:(\[redacted\]\]|[^\]\s"']*\]?)"#));

fn redact_encrypted_values(text: &str, counts: &mut RedactionCounts) -> String {
    ENCRYPTED_VALUE
        .replace_all(text, |captures: &Captures| {
            if &captures[1] == "[redacted]]" {
                return captures[0].to_string();
            }
            counts.record(Rule::EncryptedValue);
            format!("ENC[AGE:{REDACTED}]")
        })
        .into_owned()
}

/// An age identity: the prefix, then Bech32 data.
static AGE_IDENTITY: LazyLock<Regex> = LazyLock::new(|| pattern(r"AGE-SECRET-KEY-1[0-9A-Za-z]+"));

fn redact_age_identities(text: &str, counts: &mut RedactionCounts) -> String {
    AGE_IDENTITY
        .replace_all(text, |_: &Captures| {
            counts.record(Rule::AgeIdentity);
            format!("AGE-SECRET-KEY-1{REDACTED}")
        })
        .into_owned()
}

/// A scheme, `://`, then user information up to the last `@` before the
/// host. Userinfo can't hold a space, `/`, `?` or `#`, so the match never
/// runs past the authority into a path or query string.
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"([A-Za-z][A-Za-z0-9+.-]*://)([^\s/?#]+)@"));

fn redact_url_credentials(text: &str, counts: &mut RedactionCounts) -> String {
    URL_CREDENTIALS
        .replace_all(text, |captures: &Captures| {
            if &captures[2] == REDACTED {
                return captures[0].to_string();
            }
            counts.record(Rule::UrlCredentials);
            format!("{}{REDACTED}@", &captures[1])
        })
        .into_owned()
}

/// Replace every `rbrg_` token's body with [`REDACTED`].
///
/// This is the audit writer's original scanner, written by hand because a
/// regex can't say "unless it's already redacted" (Rust's `regex` has no
/// lookahead). A token is `rbrg_` and the ASCII letters, digits and
/// underscores after it, glued to a word or not.
fn redact_reliaburger_tokens(text: &str, counts: &mut RedactionCounts) -> String {
    const PREFIX: &str = "rbrg_";
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    // `find` returns a byte offset, and the prefix is ASCII, so every slice
    // below starts and ends on a character boundary.
    while let Some(start) = rest.find(PREFIX) {
        redacted.push_str(&rest[..start + PREFIX.len()]);
        let after = &rest[start + PREFIX.len()..];
        // Already redacted: copy the marker and don't count it again.
        if let Some(beyond) = after.strip_prefix(REDACTED) {
            redacted.push_str(REDACTED);
            rest = beyond;
            continue;
        }
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        redacted.push_str(REDACTED);
        counts.record(Rule::ReliaburgerToken);
        rest = &after[end..];
    }
    redacted.push_str(rest);
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Redact `text` and return the result with the counts.
    fn run(text: &str) -> (String, RedactionCounts) {
        let mut counts = RedactionCounts::default();
        let redacted = redact(text, &mut counts);
        (redacted, counts)
    }

    // Reliaburger tokens.

    #[test]
    fn a_reliaburger_token_is_redacted() {
        let (text, counts) = run("token rbrg_0123abcd_EF, then more");
        assert_eq!(text, "token rbrg_[redacted], then more");
        assert_eq!(counts.count(Rule::ReliaburgerToken), 1);
    }

    #[test]
    fn a_reliaburger_token_inside_a_word_is_still_redacted() {
        // The audit writer always redacted these, and a token glued to a
        // word (`key=rbrg_…`, `Xrbrg_…`) is still a token.
        let (text, counts) = run("key=rbrg_abc and Xrbrg_def");
        assert_eq!(text, "key=rbrg_[redacted] and Xrbrg_[redacted]");
        assert_eq!(counts.count(Rule::ReliaburgerToken), 2);
    }

    #[test]
    fn words_that_merely_look_like_the_prefix_are_left_alone() {
        let (text, counts) = run("rbrg-abc, rbrgx, hamburger_ and rb rg_");
        assert_eq!(text, "rbrg-abc, rbrgx, hamburger_ and rb rg_");
        assert!(counts.is_empty());
    }

    // Encrypted values.

    #[test]
    fn an_encrypted_value_keeps_its_prefix_and_loses_its_body() {
        let (text, counts) = run(r#"DB_PASSWORD = "ENC[AGE:YWdlLWVuY3J5cHRpb24ub3Jn+/=]""#);
        assert_eq!(text, r#"DB_PASSWORD = "ENC[AGE:[redacted]]""#);
        assert_eq!(counts.count(Rule::EncryptedValue), 1);
    }

    #[test]
    fn an_encrypted_value_cut_off_before_its_bracket_is_redacted_to_the_cut() {
        let (text, _) = run("value ENC[AGE:YWdlLWVuY3J5cHRp then more");
        assert_eq!(text, "value ENC[AGE:[redacted]] then more");
    }

    #[test]
    fn other_bracketed_values_are_left_alone() {
        let (text, counts) = run("ENC[GPG:abc] PROVIDER[vault:db/password] enc[age:abc]");
        assert_eq!(
            text,
            "ENC[GPG:abc] PROVIDER[vault:db/password] enc[age:abc]"
        );
        assert!(counts.is_empty());
    }

    // age identities.

    #[test]
    fn an_age_identity_is_redacted() {
        let identity = "AGE-SECRET-KEY-1QYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQS3290GG";
        let (text, counts) = run(&format!("# created\n{identity}\n"));
        assert_eq!(text, "# created\nAGE-SECRET-KEY-1[redacted]\n");
        assert_eq!(counts.count(Rule::AgeIdentity), 1);
    }

    #[test]
    fn an_age_recipient_is_public_and_left_alone() {
        let recipient = "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p";
        let (text, counts) = run(recipient);
        assert_eq!(text, recipient);
        assert!(counts.is_empty());
    }

    // PEM private keys.

    const PEM_BODY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7\n\
                            VJTUt9Us8cKjMzEfYyjiWA4R4/M2bS1GB4t7NXp98C3SC6dVMvDu\n";

    #[test]
    fn a_pem_private_key_is_redacted_whatever_its_algorithm() {
        for label in [
            "PRIVATE KEY",
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
        ] {
            let pem = format!("-----BEGIN {label}-----\n{PEM_BODY}-----END {label}-----");
            let (text, counts) = run(&format!("key:\n{pem}\ndone"));
            assert_eq!(
                text,
                format!("key:\n-----BEGIN {label}-----[redacted]-----END {label}-----\ndone"),
            );
            assert_eq!(counts.count(Rule::PemPrivateKey), 1, "{label}");
        }
    }

    #[test]
    fn a_pem_private_key_on_one_escaped_line_is_redacted() {
        // A key logged as a JSON string keeps its newlines as `\n`.
        let line =
            r#"{"key":"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADAN\n-----END PRIVATE KEY-----\n"}"#;
        let (text, counts) = run(line);
        assert_eq!(
            text,
            r#"{"key":"-----BEGIN PRIVATE KEY-----[redacted]-----END PRIVATE KEY-----\n"}"#
        );
        assert_eq!(counts.count(Rule::PemPrivateKey), 1);
    }

    #[test]
    fn a_pem_private_key_without_its_end_is_redacted_to_the_end_of_the_text() {
        let (text, _) = run(&format!("-----BEGIN PRIVATE KEY-----\n{PEM_BODY}"));
        assert_eq!(text, "-----BEGIN PRIVATE KEY-----[redacted]");
    }

    #[test]
    fn the_body_lines_before_an_orphaned_end_are_redacted() {
        // A log window that starts halfway through a key.
        let (text, counts) = run(&format!("{PEM_BODY}-----END PRIVATE KEY-----\nnext"));
        assert_eq!(text, "[redacted]-----END PRIVATE KEY-----\nnext");
        assert_eq!(counts.count(Rule::PemPrivateKey), 1);
    }

    #[test]
    fn public_keys_and_certificates_are_left_alone() {
        for label in ["PUBLIC KEY", "CERTIFICATE", "CERTIFICATE REQUEST"] {
            let pem = format!("-----BEGIN {label}-----\n{PEM_BODY}-----END {label}-----\n");
            let (text, counts) = run(&pem);
            assert_eq!(text, pem);
            assert!(counts.is_empty(), "{label}");
        }
    }

    // URL credentials.

    #[test]
    fn credentials_in_a_url_are_redacted_and_the_host_kept() {
        let (text, counts) = run("cloning https://deploy:hunter2@git.example.com/repo.git now");
        assert_eq!(
            text,
            "cloning https://[redacted]@git.example.com/repo.git now"
        );
        assert_eq!(counts.count(Rule::UrlCredentials), 1);
    }

    #[test]
    fn a_token_alone_as_user_information_is_redacted() {
        let (text, _) = run("postgres://ghp_abc123@db:5432/app and redis://:pw@cache:6379");
        assert_eq!(
            text,
            "postgres://[redacted]@db:5432/app and redis://[redacted]@cache:6379"
        );
    }

    #[test]
    fn a_password_with_an_at_sign_is_redacted_whole() {
        let (text, _) = run("https://user:p@ss@host/path?q=a@b");
        assert_eq!(text, "https://[redacted]@host/path?q=a@b");
    }

    #[test]
    fn urls_without_user_information_and_email_addresses_are_left_alone() {
        let line =
            "see https://example.com/a?b=c@d#e, mail bob@example.com or mailto:bob@example.com";
        let (text, counts) = run(line);
        assert_eq!(text, line);
        assert!(counts.is_empty());
    }

    // Environment maps.

    #[test]
    fn an_env_map_keeps_its_keys_and_drops_its_values() {
        let env = BTreeMap::from([
            (
                "DATABASE_URL".to_string(),
                "postgres://u:p@db/app".to_string(),
            ),
            ("LOG_LEVEL".to_string(), "debug".to_string()),
        ]);
        let mut counts = RedactionCounts::default();
        let redacted = redact_env(&env, &mut counts);
        assert_eq!(
            redacted,
            BTreeMap::from([
                ("DATABASE_URL".to_string(), "[redacted]".to_string()),
                ("LOG_LEVEL".to_string(), "[redacted]".to_string()),
            ])
        );
        assert_eq!(counts.count(Rule::EnvValue), 2);
    }

    #[test]
    fn an_empty_env_map_redacts_nothing() {
        let mut counts = RedactionCounts::default();
        let env: BTreeMap<String, String> = BTreeMap::new();
        assert!(redact_env(&env, &mut counts).is_empty());
        assert!(counts.is_empty());
    }

    // All rules together.

    #[test]
    fn counts_add_up_across_rules() {
        let (_, counts) = run(
            "rbrg_a rbrg_b ENC[AGE:x] https://u:p@h AGE-SECRET-KEY-1ABC \
             -----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
        );
        assert_eq!(counts.count(Rule::ReliaburgerToken), 2);
        assert_eq!(counts.count(Rule::EncryptedValue), 1);
        assert_eq!(counts.count(Rule::UrlCredentials), 1);
        assert_eq!(counts.count(Rule::AgeIdentity), 1);
        assert_eq!(counts.count(Rule::PemPrivateKey), 1);
        assert_eq!(counts.total(), 6);
        let rules: Vec<Rule> = counts.iter().map(|(rule, _)| rule).collect();
        assert_eq!(
            rules,
            [
                Rule::ReliaburgerToken,
                Rule::EncryptedValue,
                Rule::AgeIdentity,
                Rule::PemPrivateKey,
                Rule::UrlCredentials,
            ]
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let line = "GET /api/v1/apps 200 12ms user-agent=curl/8.4 — café ☕";
        let (text, counts) = run(line);
        assert_eq!(text, line);
        assert!(counts.is_empty());
    }

    /// The audit writer's redaction before it moved here, kept to prove the
    /// move changed nothing it did.
    fn audit_redaction_before_the_move(text: &str) -> String {
        const MARKER: &str = "rbrg_";
        let mut redacted = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(MARKER) {
            redacted.push_str(&rest[..start]);
            let after = &rest[start + MARKER.len()..];
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            redacted.push_str("rbrg_[redacted]");
            rest = &after[end..];
        }
        redacted.push_str(rest);
        redacted
    }

    /// Text built from pieces of every rule's pattern, so random strings
    /// hit the edges (half a marker, a marker already redacted, a bracket
    /// that never closes) far more often than uniform characters would.
    fn secretish_text() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just("rbrg_".to_string()),
            Just("ENC[AGE:".to_string()),
            Just("AGE-SECRET-KEY-1".to_string()),
            Just("-----BEGIN PRIVATE KEY-----".to_string()),
            Just("-----END PRIVATE KEY-----".to_string()),
            Just("-----BEGIN RSA PRIVATE KEY-----".to_string()),
            Just("https://".to_string()),
            Just("[redacted]".to_string()),
            Just("@".to_string()),
            Just("]".to_string()),
            Just("\n".to_string()),
            Just("MIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n".to_string()),
            "[a-zA-Z0-9_:/=+ .-]{0,12}",
            ".{0,8}",
        ];
        prop::collection::vec(piece, 0..24).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #[test]
        fn redaction_never_panics_and_is_idempotent(text in prop_oneof![".{0,400}", secretish_text()]) {
            let (once, _) = run(&text);
            let (twice, counts) = run(&once);
            prop_assert_eq!(&twice, &once);
            prop_assert!(counts.is_empty(), "the second pass fired {:?} on {:?}", counts, once);
        }

        #[test]
        fn audit_redaction_is_unchanged_by_the_move(text in "[a-z0-9_ rbgé,=:-]{0,200}") {
            // The alphabet has no `[`, so nothing in it can look already
            // redacted: the one place the old scanner and this one differ
            // (it redacted `rbrg_[redacted]` again; this one doesn't).
            prop_assert_eq!(redact_secrets(&text), audit_redaction_before_the_move(&text));
        }
    }
}
