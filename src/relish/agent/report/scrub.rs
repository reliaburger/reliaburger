//! Redaction for a report: the shared secret rules, then addresses.
//!
//! Secrets go first, through [`crate::redact`], the same rules the
//! `untrusted` envelopes and the audit log use; a JSON field whose key
//! names a secret (`api_key`, `password`) loses its whole value, since the
//! text rules see the key and the value apart. Then, unless the caller
//! asked to keep them, addresses are replaced with aliases that stay the
//! same across the whole report, so "`node-1` couldn't reach `ipv4-2`"
//! still says which node and which address, just not what they're called:
//!
//! - node and host names the evidence itself names, under keys such as
//!   `node_id` or `hostname`;
//! - the host of every URL;
//! - IPv4 and IPv6 addresses.
//!
//! Loopback and unspecified addresses, `localhost`, and hosts that are
//! public or reserved for examples (`github.com`, `example.com`, `*.test`)
//! are kept: they say nothing about whoever is reporting. A bare host name
//! that no field names and no URL carries isn't recognised; the manifest
//! and the human reading it are the last line of defence.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Map, Value};

use crate::redact::{self, REDACTED, RedactionCounts, Rule};

/// Keys whose string value names a node.
const NODE_KEYS: &[&str] = &[
    "node",
    "node_id",
    "node_name",
    "leader",
    "leader_id",
    "peer_id",
];

/// Keys whose string value names a host, possibly with a port.
const HOST_KEYS: &[&str] = &[
    "host",
    "hostname",
    "address",
    "advertise_address",
    "api_address",
    "bind_address",
    "endpoint",
];

/// Hosts kept as they are: public services a report links to, and the
/// names reserved for documentation (RFC 2606, RFC 6761).
const PUBLIC_HOSTS: &[&str] = &[
    "localhost",
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "docs.rs",
    "crates.io",
    "reliaburger.com",
    "example.com",
    "example.org",
    "example.net",
];

/// Suffixes reserved for documentation and testing.
const RESERVED_SUFFIXES: &[&str] = &[".example", ".invalid", ".test", ".localhost"];

/// What kind of address an alias stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AddressKind {
    Node,
    Host,
    Ipv4,
    Ipv6,
}

impl AddressKind {
    /// The alias prefix: `node-1`, `host-1`, `ipv4-1`, `ipv6-1`.
    fn prefix(self) -> &'static str {
        match self {
            AddressKind::Node => "node",
            AddressKind::Host => "host",
            AddressKind::Ipv4 => "ipv4",
            AddressKind::Ipv6 => "ipv6",
        }
    }

    /// The rule's name in the manifest.
    fn rule_name(self) -> &'static str {
        match self {
            AddressKind::Node => "node_name",
            AddressKind::Host => "hostname",
            AddressKind::Ipv4 => "ipv4_address",
            AddressKind::Ipv6 => "ipv6_address",
        }
    }
}

/// A shared secret rule's name in the manifest.
fn rule_name(rule: Rule) -> &'static str {
    rule.name()
}

/// What redaction did to one item of the report.
#[derive(Debug, Clone, Default)]
pub(super) struct ItemCounts {
    rules: BTreeMap<String, usize>,
    /// Log lines dropped because `--include-logs` wasn't given.
    pub(super) log_lines_left_out: usize,
}

impl ItemCounts {
    fn add(&mut self, rule: &str, count: usize) {
        if count > 0 {
            *self.rules.entry(rule.to_string()).or_insert(0) += count;
        }
    }

    fn add_secrets(&mut self, counts: &RedactionCounts) {
        for (rule, count) in counts.iter() {
            self.add(rule_name(rule), count);
        }
    }

    /// Rule name to count, for the manifest.
    pub(super) fn into_map(self) -> BTreeMap<String, usize> {
        self.rules
    }
}

/// The redactor for one report. It remembers every alias it hands out, so
/// the same address has the same alias in every file.
pub(super) struct Scrubber {
    keep_addresses: bool,
    include_logs: bool,
    /// Names the evidence gave for nodes and hosts, longest first.
    known: Vec<(String, AddressKind)>,
    /// One pattern matching any of `known`, when there are any.
    known_pattern: Option<Regex>,
    /// Real value to alias.
    aliases: BTreeMap<String, String>,
    /// How many aliases of each kind have been handed out.
    issued: BTreeMap<AddressKind, usize>,
}

impl Scrubber {
    /// A scrubber that knows the node and host names in `evidence`.
    pub(super) fn new(keep_addresses: bool, include_logs: bool, evidence: &[&Value]) -> Self {
        let mut known = BTreeMap::new();
        for value in evidence {
            learn_names(value, &mut known);
        }
        let mut known: Vec<(String, AddressKind)> = known.into_iter().collect();
        // Longest first, so `wolf4.lan` matches before `wolf4`.
        known.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let known_pattern = (!known.is_empty()).then(|| {
            let names: Vec<String> = known.iter().map(|(name, _)| regex::escape(name)).collect();
            pattern(&format!(r"\b(?:{})\b", names.join("|")))
        });
        Scrubber {
            keep_addresses,
            include_logs,
            known,
            known_pattern,
            aliases: BTreeMap::new(),
            issued: BTreeMap::new(),
        }
    }

    /// `text` with secrets redacted and addresses aliased.
    pub(super) fn text(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        let text = self.secrets(text, counts);
        if self.keep_addresses {
            return text;
        }
        let text = self.known_names(&text, counts);
        let text = self.url_hosts(&text, counts);
        let text = self.ipv4(&text, counts);
        self.ipv6(&text, counts)
    }

    /// A patch: secrets only. A patch is code, and aliasing an address in
    /// a test would change what the test does.
    pub(super) fn patch(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        self.secrets(text, counts)
    }

    fn secrets(&self, text: &str, counts: &mut ItemCounts) -> String {
        let mut secrets = RedactionCounts::default();
        let text = redact::redact(text, &mut secrets);
        counts.add_secrets(&secrets);
        text
    }

    /// `value` with every string scrubbed, every `env` map reduced to its
    /// keys, and workload log lines dropped unless logs were asked for.
    pub(super) fn json(&mut self, value: Value, counts: &mut ItemCounts) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(&text, counts)),
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| self.json(item, counts))
                    .collect(),
            ),
            Value::Object(map) => {
                let mut scrubbed = Map::new();
                for (key, value) in map {
                    let value = match (key.as_str(), value) {
                        ("env", Value::Object(env)) => env_keys(&env, counts),
                        (name, Value::String(text)) if redact::is_secret_key(name) => {
                            let mut secrets = RedactionCounts::default();
                            let text = redact::redact_value(name, &text, &mut secrets);
                            counts.add_secrets(&secrets);
                            Value::String(self.text(&text, counts))
                        }
                        ("untrusted", Value::Object(text)) if !self.include_logs => {
                            self.json(drop_log_lines(text, counts), counts)
                        }
                        (_, value) => self.json(value, counts),
                    };
                    scrubbed.insert(self.text(&key, counts), value);
                }
                Value::Object(scrubbed)
            }
            other => other,
        }
    }

    /// The alias for `real`, handing out the next one of `kind` the first
    /// time it's seen.
    fn alias(&mut self, real: &str, kind: AddressKind, counts: &mut ItemCounts) -> String {
        counts.add(kind.rule_name(), 1);
        if let Some(alias) = self.aliases.get(real) {
            return alias.clone();
        }
        let issued = self.issued.entry(kind).or_insert(0);
        *issued += 1;
        let alias = format!("{}-{issued}", kind.prefix());
        self.aliases.insert(real.to_string(), alias.clone());
        alias
    }

    fn known_names(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        let Some(pattern) = self.known_pattern.clone() else {
            return text.to_string();
        };
        pattern
            .replace_all(text, |captures: &Captures| {
                let name = &captures[0];
                let kind = self
                    .known
                    .iter()
                    .find(|(known, _)| known == name)
                    .map_or(AddressKind::Host, |(_, kind)| *kind);
                self.alias(name, kind, counts)
            })
            .into_owned()
    }

    fn url_hosts(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        URL_HOST
            .replace_all(text, |captures: &Captures| {
                let host = &captures[2];
                let kept = is_public_host(host)
                    || is_alias(host)
                    || host.starts_with('[')
                    || host.parse::<Ipv4Addr>().is_ok();
                if kept {
                    return captures[0].to_string();
                }
                format!(
                    "{}{}",
                    &captures[1],
                    self.alias(host, AddressKind::Host, counts)
                )
            })
            .into_owned()
    }

    fn ipv4(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        DOTTED_NUMBERS
            .replace_all(text, |captures: &Captures| {
                let candidate = &captures[0];
                match candidate.parse::<Ipv4Addr>() {
                    Ok(address) if !(address.is_loopback() || address.is_unspecified()) => {
                        self.alias(candidate, AddressKind::Ipv4, counts)
                    }
                    _ => candidate.to_string(),
                }
            })
            .into_owned()
    }

    fn ipv6(&mut self, text: &str, counts: &mut ItemCounts) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for found in IPV6_CANDIDATE.find_iter(text) {
            let candidate = found.as_str();
            // A hex run glued to a word (`Self::dead`) is code, not an
            // address; the regex crate has no lookbehind, so check here.
            let glued_before = text[..found.start()]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let glued_after = text[found.end()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let address = candidate.parse::<Ipv6Addr>();
            let replace = candidate.matches(':').count() >= 2
                && !glued_before
                && !glued_after
                && address
                    .is_ok_and(|address| !(address.is_loopback() || address.is_unspecified()));
            if replace {
                out.push_str(&text[last..found.start()]);
                out.push_str(&self.alias(candidate, AddressKind::Ipv6, counts));
                last = found.end();
            }
        }
        out.push_str(&text[last..]);
        out
    }
}

/// `scheme://`, optional user information (already redacted by then), and
/// the host: a bracketed IPv6 literal or a name up to the port or path.
static URL_HOST: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"([A-Za-z][A-Za-z0-9+.-]*://(?:[^\s/?#@]*@)?)(\[[^\]\s]*\]|[^\s/?#:@\[\]'`<>]+)")
});

/// A run of numbers joined by dots; an IPv4 address when it has exactly
/// four parts that fit in a byte, which `Ipv4Addr::from_str` checks. Taking
/// the whole run means `1.2.3.4.5` is left alone rather than half-aliased.
static DOTTED_NUMBERS: LazyLock<Regex> = LazyLock::new(|| pattern(r"\b[0-9]+(?:\.[0-9]+)+\b"));

/// Hex digits, colons and dots (for `::ffff:10.0.0.1`), as long as an
/// IPv6 address can be; `Ipv6Addr::from_str` decides.
static IPV6_CANDIDATE: LazyLock<Regex> = LazyLock::new(|| pattern(r"[0-9A-Fa-f:.]{2,45}"));

/// Compile one of this module's patterns.
///
/// Each is a literal the tests below compile, or a list of escaped names,
/// so a typo fails `cargo test`, never a report; the `expect` can't fire in
/// a binary that passed them.
fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the report's address patterns are valid regexes")
}

fn is_public_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    PUBLIC_HOSTS.contains(&host.as_str())
        || host.ends_with(".github.com")
        || [".example.com", ".example.org", ".example.net"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        || RESERVED_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

/// Whether `name` is one of our own aliases, so a second pass leaves it.
fn is_alias(name: &str) -> bool {
    name.split_once('-').is_some_and(|(prefix, number)| {
        ["node", "host", "ipv4", "ipv6"].contains(&prefix)
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
    })
}

/// Add every node and host name `value` gives under a key that names one.
fn learn_names(value: &Value, known: &mut BTreeMap<String, AddressKind>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| learn_names(item, known)),
        Value::Object(map) => {
            for (key, value) in map {
                let kind = if NODE_KEYS.contains(&key.as_str()) {
                    Some(AddressKind::Node)
                } else if HOST_KEYS.contains(&key.as_str()) {
                    Some(AddressKind::Host)
                } else {
                    None
                };
                match (kind, value) {
                    (Some(kind), Value::String(name)) => {
                        if let Some(name) = host_name(name) {
                            known.entry(name.to_string()).or_insert(kind);
                        }
                    }
                    (_, value) => learn_names(value, known),
                }
            }
        }
        _ => {}
    }
}

/// The name in a `host`, `host:port` or `name` value worth aliasing, or
/// `None` for an IP address (its own rule handles it), a URL (likewise), a
/// public host, or something too short to replace safely.
fn host_name(value: &str) -> Option<&str> {
    if value.contains("://") || value.contains(char::is_whitespace) {
        return None;
    }
    let name = match value.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => value,
    };
    let usable = name.len() >= 3
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.ends_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && name.parse::<Ipv4Addr>().is_err()
        && !name.contains(REDACTED)
        && !is_public_host(name)
        && !is_alias(name);
    usable.then_some(name)
}

/// An `env` map with every value replaced, through the shared rule.
fn env_keys(env: &Map<String, Value>, counts: &mut ItemCounts) -> Value {
    // A value that's already the marker isn't counted again, so a report
    // built from another report's bundle doesn't claim secrets it never saw.
    let mut secrets = RedactionCounts::default();
    let fresh = env
        .iter()
        .filter(|(_, value)| value.as_str() != Some(REDACTED));
    let mut keys = redact::redact_env(fresh, &mut secrets);
    for key in env.keys() {
        keys.entry(key.clone())
            .or_insert_with(|| REDACTED.to_string());
    }
    counts.add_secrets(&secrets);
    Value::Object(
        keys.into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect(),
    )
}

/// An `untrusted` object without its lines when they're a workload's log,
/// marked truncated and counted.
fn drop_log_lines(mut text: Map<String, Value>, counts: &mut ItemCounts) -> Value {
    if text.get("origin").and_then(Value::as_str) == Some("workload_log")
        && let Some(Value::Array(lines)) = text.get_mut("lines")
        && !lines.is_empty()
    {
        counts.log_lines_left_out += lines.len();
        lines.clear();
        text.insert("truncated".to_string(), Value::Bool(true));
    }
    Value::Object(text)
}
