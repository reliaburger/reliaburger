//! Decision provenance: the rule behind a placement, a restart or a refused
//! deploy, and the evidence it compared, recorded where the decision is made.
//!
//! A decision travels as an ordinary event (or audit record) whose
//! `details` carry a `rule` key naming one of [`RULES`] and one key per
//! piece of evidence. Nothing new crosses the wire: `details` has always
//! been a string map that old and new nodes read alike. `relish agent
//! explain` and `relish agent timeline` read the rule back and look its
//! meaning up here.
//!
//! The catalogue is the contract. A test reads the embedded source and
//! checks every rule listed here is recorded by exactly one function, and
//! every rule the code records is listed here, so the two can't drift.

use std::collections::BTreeMap;
use std::fmt::Display;

use schemars::JsonSchema;
use serde::Serialize;

/// The `details` key that names the rule.
pub const RULE_KEY: &str = "rule";

/// The longest an evidence value may be, in bytes. Longer values are cut at
/// a character boundary and end in `…`.
pub const EVIDENCE_VALUE_BYTES_MAX: usize = 200;

/// The most evidence keys one decision carries, besides `rule`. Later keys
/// are dropped and `evidence_dropped` says how many.
pub const EVIDENCE_KEYS_MAX: usize = 16;

/// What a decision was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Where the scheduler put an app's replicas, or why it couldn't.
    Placement,
    /// Why a node restarted an instance, or gave up on it.
    Restart,
    /// Why the cluster refused a deploy.
    Refusal,
}

/// One rule a decision can name.
#[derive(Debug, Clone, Copy)]
pub struct DecisionRule {
    /// The stable id recorded under [`RULE_KEY`].
    pub id: &'static str,
    /// What it's about.
    pub kind: DecisionKind,
    /// What the rule says, in one or two sentences.
    pub meaning: &'static str,
    /// The evidence keys it records, each with what it means. `<instance>`
    /// stands for an instance name such as `web-1`.
    pub evidence: &'static [(&'static str, &'static str)],
}

const RESTART_EVIDENCE: &[(&str, &str)] = &[
    ("instance", "the instance restarted"),
    ("attempt", "which restart this is, counting from 1"),
    (
        "restart_limit",
        "the app's restart budget, or `unlimited` for a long-running app",
    ),
    (
        "backoff_seconds",
        "how long the node waited before this restart",
    ),
];

/// Every rule a decision can name.
pub const RULES: &[DecisionRule] = &[
    DecisionRule {
        id: "placement.scored",
        kind: DecisionKind::Placement,
        meaning: "the scheduler filtered the nodes that can run the app, then picked the one \
                  running the fewest of its replicas and, among those, the highest score: \
                  bin-packing 50, preferred labels 20, image already cached 15, uptime 5",
        evidence: &[
            (
                "<instance>",
                "the node chosen, the replicas already there, its score and parts, how many \
                 nodes passed the filter, and the runner-up",
            ),
            ("wanted", "the replicas the app wants"),
            (
                "kept",
                "replicas that stayed where they were, so weren't decided again",
            ),
        ],
    },
    DecisionRule {
        id: "placement.daemon",
        kind: DecisionKind::Placement,
        meaning: "a daemon set runs one replica on every node that passes the filter; there's \
                  no scoring",
        evidence: &[
            ("<instance>", "the node it runs on"),
            ("eligible", "how many nodes passed the filter"),
        ],
    },
    DecisionRule {
        id: "placement.volume_home",
        kind: DecisionKind::Placement,
        meaning: "an app with managed volumes that had no replica left goes back to the nodes \
                  that hold its volumes, without scoring",
        evidence: &[("<instance>", "the node that holds its volumes")],
    },
    DecisionRule {
        id: "placement.short",
        kind: DecisionKind::Placement,
        meaning: "some replicas found no node: the first hard constraint no node met is the \
                  reason",
        evidence: &[
            (
                "reason",
                "no_ready_node, no_label_match, no_container_runtime, no_capable_node or no_room",
            ),
            ("placed", "replicas that have a node"),
            ("wanted", "replicas the app wants"),
        ],
    },
    DecisionRule {
        id: "placement.quota_blocked",
        kind: DecisionKind::Placement,
        meaning: "the app's namespace quota has no room for it, so none of its new replicas \
                  were placed",
        evidence: &[("reason", "the quota that has no room")],
    },
    DecisionRule {
        id: "restart.process_exited",
        kind: DecisionKind::Restart,
        meaning: "the instance's process was no longer running, so the node restarted it, \
                  within the restart budget and after the backoff",
        evidence: RESTART_EVIDENCE,
    },
    DecisionRule {
        id: "restart.start_failed",
        kind: DecisionKind::Restart,
        meaning: "the runtime couldn't start the instance on an earlier attempt, so the node \
                  cleaned up and tried again, within the restart budget and after the backoff",
        evidence: RESTART_EVIDENCE,
    },
    DecisionRule {
        id: "restart.health_failed",
        kind: DecisionKind::Restart,
        meaning: "the instance failed its health check `threshold` times in a row, so the node \
                  withdrew it from service and restarted it",
        evidence: &[
            ("instance", "the instance restarted"),
            ("attempt", "which restart this is, counting from 1"),
            (
                "restart_limit",
                "the app's restart budget, or `unlimited` for a long-running app",
            ),
            (
                "backoff_seconds",
                "how long the node waited before this restart",
            ),
            (
                "consecutive_failures",
                "failed probes in a row when it was marked unhealthy",
            ),
            (
                "threshold",
                "failed probes in a row that make it unhealthy (`threshold_unhealthy`)",
            ),
            ("probe", "what was probed: protocol, port and path"),
            ("interval_seconds", "seconds between probes"),
        ],
    },
    DecisionRule {
        id: "restart.budget_exhausted",
        kind: DecisionKind::Restart,
        meaning: "the instance stopped again after using its whole restart budget, so the node \
                  gave up and marked it failed",
        evidence: &[
            ("instance", "the instance that failed"),
            ("attempts", "restarts it had"),
            ("restart_limit", "the app's restart budget"),
        ],
    },
    DecisionRule {
        id: "restart.sidecar_exited",
        kind: DecisionKind::Restart,
        meaning: "a sidecar's process exited while its instance kept running, so the node \
                  restarted the sidecar alone",
        evidence: &[
            ("instance", "the instance the sidecar belongs to"),
            ("sidecar", "the sidecar"),
            ("attempt", "the sidecar's restarts so far"),
        ],
    },
    DecisionRule {
        id: "deploy.refused.policy",
        kind: DecisionKind::Refusal,
        meaning: "an enforcing policy denied the apply, so nothing in it was written",
        evidence: &[
            ("policies", "the policies that denied it"),
            ("workloads", "the workloads they denied, as name@namespace"),
        ],
    },
    DecisionRule {
        id: "deploy.refused.owner",
        kind: DecisionKind::Refusal,
        meaning: "the apply declared an owner the caller can't speak for, or a different owner \
                  from the app's, so nothing in it was written",
        evidence: &[
            ("workloads", "the app refused, as name@namespace"),
            ("reason", "not_the_caller or claimed"),
        ],
    },
    DecisionRule {
        id: "deploy.refused.council",
        kind: DecisionKind::Refusal,
        meaning: "the council's state machine refused the write (a lease expired, a resource \
                  owned elsewhere, a quota), so the apply stopped there",
        evidence: &[
            ("workloads", "the app refused, as name@namespace"),
            ("reason", "the state machine's reason"),
        ],
    },
];

/// The rule `id`, if the catalogue has it.
pub fn rule(id: &str) -> Option<&'static DecisionRule> {
    RULES.iter().find(|rule| rule.id == id)
}

/// A decision being recorded: its rule and the evidence, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    details: BTreeMap<String, String>,
    dropped: usize,
}

impl Decision {
    /// A decision under `rule`, which should be one of [`RULES`]. It takes a
    /// `&'static str` so the id is a literal the catalogue test can find.
    pub fn new(rule: &'static str) -> Self {
        Decision {
            details: BTreeMap::from([(RULE_KEY.to_string(), rule.to_string())]),
            dropped: 0,
        }
    }

    /// Add one piece of evidence. Past [`EVIDENCE_KEYS_MAX`] it's counted,
    /// not kept, and a value over [`EVIDENCE_VALUE_BYTES_MAX`] is cut.
    pub fn with(mut self, key: impl Into<String>, value: impl Display) -> Self {
        let key = key.into();
        // `rule` is one of the entries, so the evidence limit is one more.
        if self.details.len() > EVIDENCE_KEYS_MAX && !self.details.contains_key(&key) {
            self.dropped += 1;
            return self;
        }
        self.details.insert(key, bounded(value.to_string()));
        self
    }

    /// The `details` map to record.
    pub fn into_details(mut self) -> BTreeMap<String, String> {
        if self.dropped > 0 {
            self.details
                .insert("evidence_dropped".to_string(), self.dropped.to_string());
        }
        self.details
    }
}

/// `value`, cut to [`EVIDENCE_VALUE_BYTES_MAX`] bytes at a character
/// boundary.
fn bounded(mut value: String) -> String {
    if value.len() <= EVIDENCE_VALUE_BYTES_MAX {
        return value;
    }
    let ellipsis = '…';
    let mut end = EVIDENCE_VALUE_BYTES_MAX - ellipsis.len_utf8();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push(ellipsis);
    value
}

/// An app's `name@namespace`, as the `workloads` evidence lists it.
pub fn workload(app: &str, namespace: &str) -> String {
    format!("{app}@{namespace}")
}

/// The rule a recorded event or audit record names, if it names one.
pub fn rule_of(details: &BTreeMap<String, String>) -> Option<&str> {
    details.get(RULE_KEY).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_records_its_rule_and_evidence() {
        let details = Decision::new("restart.process_exited")
            .with("instance", "web-0")
            .with("attempt", 2)
            .into_details();
        assert_eq!(
            details,
            BTreeMap::from([
                ("rule".to_string(), "restart.process_exited".to_string()),
                ("instance".to_string(), "web-0".to_string()),
                ("attempt".to_string(), "2".to_string()),
            ])
        );
        assert_eq!(rule_of(&details), Some("restart.process_exited"));
    }

    #[test]
    fn evidence_past_the_key_limit_is_counted_not_kept() {
        let mut decision = Decision::new("placement.scored");
        for index in 0..EVIDENCE_KEYS_MAX + 3 {
            decision = decision.with(format!("web-{index}"), "node-1");
        }
        let details = decision.into_details();
        // The rule, the cap's worth of evidence and the count.
        assert_eq!(details.len(), EVIDENCE_KEYS_MAX + 2);
        assert_eq!(details["evidence_dropped"], "3");
    }

    #[test]
    fn a_long_value_is_cut_at_a_character_boundary() {
        let details = Decision::new("deploy.refused.council")
            .with("reason", "é".repeat(EVIDENCE_VALUE_BYTES_MAX))
            .into_details();
        let reason = &details["reason"];
        assert!(reason.len() <= EVIDENCE_VALUE_BYTES_MAX, "{}", reason.len());
        assert!(reason.ends_with('…'));
    }

    #[test]
    fn rule_ids_are_unique_and_name_their_kind() {
        let mut seen = std::collections::BTreeSet::new();
        for rule in RULES {
            assert!(seen.insert(rule.id), "{} is listed twice", rule.id);
            let prefix = match rule.kind {
                DecisionKind::Placement => "placement.",
                DecisionKind::Restart => "restart.",
                DecisionKind::Refusal => "deploy.refused.",
            };
            assert!(rule.id.starts_with(prefix), "{}", rule.id);
            assert!(!rule.evidence.is_empty(), "{} has no evidence", rule.id);
        }
    }
}
