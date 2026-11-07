//! Every finding `relish wtf` can report: its id, what it means, and the
//! thresholds that decide it.
//!
//! The thresholds are the constants [`super::diagnose`] compares against, so
//! `relish agent explain` reads the same number the check uses and the two
//! can't drift. Where the check is decided in the source is found at run time
//! by searching the embedded source for the id, not written down here.

use schemars::JsonSchema;
use serde::Serialize;

use crate::relish::council_view::VOTER_LAG_WARNING;
use crate::sesame::token::{ADMIN_TOKEN_AGE_WARNING, TOKEN_EXPIRY_WARNING};

/// `crashloop`: this many restarts inside [`CRASHLOOP_WINDOW_SECONDS`].
pub const CRASHLOOP_RESTARTS: usize = 3;
/// `crashloop`: the window restarts are counted in.
pub const CRASHLOOP_WINDOW_SECONDS: u64 = 15 * 60;
/// `crashloop`: a deploy this recent is named as the likely cause.
pub const RECENT_DEPLOY_SECONDS: u64 = 30 * 60;
/// `deploy-stuck`: an operation still active after this long.
pub const STUCK_DEPLOY_SECONDS: u64 = 15 * 60;
/// `cert-expiring`: a certificate expiring within this, without healthy
/// automatic rotation.
pub const CERTIFICATE_WARNING_SECONDS: u64 = 14 * 24 * 60 * 60;
/// `disk-high`: a warning from this percentage full.
pub const DISK_WARNING_PERCENT: u64 = 85;
/// `disk-high`: critical from this percentage full.
pub const DISK_CRITICAL_PERCENT: u64 = 95;

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Needs action now: something is down or about to be.
    Critical,
    /// Needs attention, but doesn't prove an outage.
    Warning,
}

/// What a threshold's number counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// A plain count.
    Count,
    /// Seconds.
    Seconds,
    /// Percent of capacity.
    Percent,
}

/// One number a check compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    /// The constant's name in the source, e.g. `CRASHLOOP_RESTARTS`.
    pub constant: &'static str,
    /// Its value.
    pub value: u64,
    /// What the value counts.
    pub unit: Unit,
    /// What the number decides, in one line.
    pub meaning: &'static str,
}

/// One kind of finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindingKind {
    /// The id `wtf` reports, e.g. `crashloop`.
    pub id: &'static str,
    /// The severities it can be reported at.
    pub severities: &'static [Severity],
    /// What it means, in a sentence or two.
    pub meaning: &'static str,
    /// The numbers that decide it; empty when the check is a plain fact.
    pub thresholds: &'static [Threshold],
}

const CRITICAL: &[Severity] = &[Severity::Critical];
const WARNING: &[Severity] = &[Severity::Warning];

/// Every finding `wtf` reports, in the order its checks run.
pub const FINDINGS: &[FindingKind] = &[
    FindingKind {
        id: "unreachable-nodes",
        severities: CRITICAL,
        meaning: "a member node's Bun API didn't answer the authenticated requests wtf sends through the entry node",
        thresholds: &[],
    },
    FindingKind {
        id: "node-dead",
        severities: CRITICAL,
        meaning: "Mustard gossip marks the node dead or suspect: the cluster no longer counts it as alive",
        thresholds: &[],
    },
    FindingKind {
        id: "version-skew",
        severities: WARNING,
        meaning: "the nodes run different bun builds (version and commit, or the binary's SHA-256 when the commit is unknown)",
        thresholds: &[],
    },
    FindingKind {
        id: "os-update-failed",
        severities: WARNING,
        meaning: "an appliance couldn't update its OS during `relish os upgrade`; the rollout stops at that node until `relish os resume`",
        thresholds: &[],
    },
    FindingKind {
        id: "os-skew",
        severities: WARNING,
        meaning: "the appliances run different Reliaburger OS versions, as they do partway through a rollout",
        thresholds: &[],
    },
    FindingKind {
        id: "council-recovery-split",
        severities: CRITICAL,
        meaning: "two `relish council recover` runs landed on the same recovery epoch, so both councils fenced themselves and refuse writes",
        thresholds: &[],
    },
    FindingKind {
        id: "council-fence-unpersisted",
        severities: WARNING,
        meaning: "a node couldn't write its recovery fence to disk; restarting it before the write lands brings it back unfenced",
        thresholds: &[],
    },
    FindingKind {
        id: "council-fenced",
        severities: CRITICAL,
        meaning: "nodes belong to a council that `relish council recover` replaced; they serve no Raft and refuse writes until re-enrolled",
        thresholds: &[],
    },
    FindingKind {
        id: "council-epoch-split",
        severities: CRITICAL,
        meaning: "nodes serve different recovery epochs: there are two councils",
        thresholds: &[],
    },
    FindingKind {
        id: "council-voter-lagging",
        severities: WARNING,
        meaning: "a council voter answers but has applied fewer log entries than the others, as a voter with a full disk does",
        thresholds: &[Threshold {
            constant: "VOTER_LAG_WARNING",
            value: VOTER_LAG_WARNING,
            unit: Unit::Count,
            meaning: "applied log entries behind the furthest voter",
        }],
    },
    FindingKind {
        id: "council-multiple-leaders",
        severities: CRITICAL,
        meaning: "more than one node reports itself, or is reported, as the council leader",
        thresholds: &[],
    },
    FindingKind {
        id: "no-leader",
        severities: CRITICAL,
        meaning: "the council has no elected leader, so the cluster can't commit changes",
        thresholds: &[],
    },
    FindingKind {
        id: "quorum-loss",
        severities: CRITICAL,
        meaning: "fewer council members answer than a majority (member_count / 2 + 1), so Raft can't commit",
        thresholds: &[],
    },
    FindingKind {
        id: "council-member-down",
        severities: WARNING,
        meaning: "a council member doesn't answer but quorum holds; the detail says how many more failures would lose it",
        thresholds: &[],
    },
    FindingKind {
        id: "active-faults",
        severities: WARNING,
        meaning: "Smoker faults are injected and still active, so what you see may be a drill",
        thresholds: &[],
    },
    FindingKind {
        id: "disk-high",
        severities: &[Severity::Warning, Severity::Critical],
        meaning: "a node's filesystem for one or more storage domains is nearly full",
        thresholds: &[
            Threshold {
                constant: "DISK_WARNING_PERCENT",
                value: DISK_WARNING_PERCENT,
                unit: Unit::Percent,
                meaning: "used space that makes it a warning",
            },
            Threshold {
                constant: "DISK_CRITICAL_PERCENT",
                value: DISK_CRITICAL_PERCENT,
                unit: Unit::Percent,
                meaning: "used space that makes it critical",
            },
        ],
    },
    FindingKind {
        id: "cert-expired",
        severities: CRITICAL,
        meaning: "a node or workload certificate is past its not-after time",
        thresholds: &[],
    },
    FindingKind {
        id: "cert-expiring",
        severities: WARNING,
        meaning: "a certificate expires soon and nothing shows it rotating automatically",
        thresholds: &[Threshold {
            constant: "CERTIFICATE_WARNING_SECONDS",
            value: CERTIFICATE_WARNING_SECONDS,
            unit: Unit::Seconds,
            meaning: "time left before expiry that starts the warning",
        }],
    },
    FindingKind {
        id: "registry-lost-images",
        severities: CRITICAL,
        meaning: "some layer of an image has no live copy anywhere, so the image can't be pulled or deployed until it's pushed again",
        thresholds: &[],
    },
    FindingKind {
        id: "registry-needs-mtls",
        severities: WARNING,
        meaning: "the cluster runs plaintext transports, so the registry can't replicate or reach its leader",
        thresholds: &[],
    },
    FindingKind {
        id: "registry-redundancy",
        severities: WARNING,
        meaning: "a node's registry isn't listening, peers can't reach it, or layers have fewer live copies than [images] redundancy asks for",
        thresholds: &[],
    },
    FindingKind {
        id: "token-expired",
        severities: WARNING,
        meaning: "an API token is past its expiry; clients using it are refused",
        thresholds: &[],
    },
    FindingKind {
        id: "token-expiring",
        severities: WARNING,
        meaning: "an API token expires soon",
        thresholds: &[Threshold {
            constant: "TOKEN_EXPIRY_WARNING",
            value: TOKEN_EXPIRY_WARNING.as_secs(),
            unit: Unit::Seconds,
            meaning: "time left before expiry that starts the warning",
        }],
    },
    FindingKind {
        id: "admin-token-old",
        severities: WARNING,
        meaning: "an admin token never expires and its secret is old enough to rotate",
        thresholds: &[Threshold {
            constant: "ADMIN_TOKEN_AGE_WARNING",
            value: ADMIN_TOKEN_AGE_WARNING.as_secs(),
            unit: Unit::Seconds,
            meaning: "age of the secret that starts the warning",
        }],
    },
    FindingKind {
        id: "namespace-isolation",
        severities: WARNING,
        meaning: "apps span more than one namespace and a node can't keep them apart (no live eBPF hooks), so any namespace reaches any other there",
        thresholds: &[],
    },
    FindingKind {
        id: "gitops-plaintext-secret",
        severities: WARNING,
        meaning: "a secret-looking value is plaintext in the GitOps repository",
        thresholds: &[],
    },
    FindingKind {
        id: "gitops-local-config",
        severities: WARNING,
        meaning: "a node's own [gitops] differs from the cluster's and is ignored",
        thresholds: &[],
    },
    FindingKind {
        id: "gitops-config-unreadable",
        severities: WARNING,
        meaning: "a node can't unseal the cluster's GitOps credentials, so as leader it couldn't sync",
        thresholds: &[],
    },
    FindingKind {
        id: "policy-broken",
        severities: WARNING,
        meaning: "a policy in force can't be evaluated, so every app apply is refused",
        thresholds: &[],
    },
    FindingKind {
        id: "policy-would-deny",
        severities: WARNING,
        meaning: "a committed app or job breaks a deny policy; it keeps running, but its next apply is refused",
        thresholds: &[],
    },
    FindingKind {
        id: "policy-audit",
        severities: WARNING,
        meaning: "a committed app or job breaks an audit policy, which only records",
        thresholds: &[],
    },
    FindingKind {
        id: "crashloop",
        severities: CRITICAL,
        meaning: "an app restarted repeatedly in a short window; a recent deploy and the app's first error line are attached when there are any",
        thresholds: &[
            Threshold {
                constant: "CRASHLOOP_RESTARTS",
                value: CRASHLOOP_RESTARTS as u64,
                unit: Unit::Count,
                meaning: "restarts inside the window that make a crash loop",
            },
            Threshold {
                constant: "CRASHLOOP_WINDOW_SECONDS",
                value: CRASHLOOP_WINDOW_SECONDS,
                unit: Unit::Seconds,
                meaning: "the window restarts are counted in",
            },
            Threshold {
                constant: "RECENT_DEPLOY_SECONDS",
                value: RECENT_DEPLOY_SECONDS,
                unit: Unit::Seconds,
                meaning: "a deploy started this recently is named as the likely cause",
            },
        ],
    },
    FindingKind {
        id: "no-backends",
        severities: CRITICAL,
        meaning: "a service wants replicas but none of its backends is healthy",
        thresholds: &[],
    },
    FindingKind {
        id: "launch-failed",
        severities: CRITICAL,
        meaning: "the runtime refused to launch an app's instances (a missing binary, say); waiting won't start them",
        thresholds: &[],
    },
    FindingKind {
        id: "quota-blocked",
        severities: WARNING,
        meaning: "an app isn't placed because its namespace quota has no room",
        thresholds: &[],
    },
    FindingKind {
        id: "volume-home-away",
        severities: CRITICAL,
        meaning: "a managed-volume app waits for the node holding its volume, which is out of the cluster",
        thresholds: &[],
    },
    FindingKind {
        id: "unschedulable",
        severities: WARNING,
        meaning: "the scheduler placed only some of an app's replicas and recorded why the rest have no node",
        thresholds: &[],
    },
    FindingKind {
        id: "under-replicated",
        severities: WARNING,
        meaning: "an app runs fewer replicas than it wants, for no reason the scheduler recorded",
        thresholds: &[],
    },
    FindingKind {
        id: "rollout-failed",
        severities: WARNING,
        meaning: "an app's current spec failed to roll out on a node; the leader holds its other nodes on the previous spec",
        thresholds: &[],
    },
    FindingKind {
        id: "deploy-stuck",
        severities: WARNING,
        meaning: "a deploy operation has stayed active for too long",
        thresholds: &[Threshold {
            constant: "STUCK_DEPLOY_SECONDS",
            value: STUCK_DEPLOY_SECONDS,
            unit: Unit::Seconds,
            meaning: "how long an active deploy may run before it counts as stuck",
        }],
    },
    FindingKind {
        id: "alerts-firing",
        severities: WARNING,
        meaning: "a Mayo alert rule is firing",
        thresholds: &[],
    },
    FindingKind {
        id: "cpu-throttling",
        severities: WARNING,
        meaning: "an app's cgroup accumulated throttled CPU time over the observation window: it wants more CPU than its limit",
        thresholds: &[],
    },
];

/// The finding with this id, if `wtf` has one.
pub fn finding(id: &str) -> Option<&'static FindingKind> {
    FINDINGS.iter().find(|kind| kind.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for kind in FINDINGS {
            assert!(seen.insert(kind.id), "{} twice", kind.id);
            assert!(
                kind.id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}",
                kind.id
            );
            assert!(!kind.severities.is_empty(), "{}", kind.id);
        }
    }

    #[test]
    fn a_known_id_is_found_and_an_unknown_one_is_not() {
        assert_eq!(finding("crashloop").map(|kind| kind.id), Some("crashloop"));
        assert!(finding("crashloops").is_none(), "that's an OK row's id");
    }
}
