//! Every check `relish wtf` runs: which findings and unknowns belong to it,
//! what exactly it tests, and where the manual covers it.
//!
//! A check is one row of the default table. Its OK row carries the check's
//! id, its findings carry their own ids (see [`super::findings`]), and an
//! UNKNOWN carries the evidence source it couldn't read. This catalogue ties
//! the three together, so the table can show one row per check and
//! `--explain` can say what each one tests.

/// Where the manual is published: the chapters on GitHub.
pub const MANUAL_URL: &str = "https://github.com/reliaburger/reliaburger/blob/main/docs/manual/";

/// One check `wtf` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckKind {
    /// The row's name, and the id of its OK result, e.g. `council`.
    pub id: &'static str,
    /// The finding ids this check reports.
    pub findings: &'static [&'static str],
    /// The evidence sources whose UNKNOWN rows belong to this check.
    pub sources: &'static [&'static str],
    /// What the check reads and the rule it applies, in a sentence or two.
    pub tests: &'static str,
    /// The manual chapter file and heading anchor that cover it, e.g.
    /// `12_operations.md#gitops`.
    pub docs: &'static str,
    /// The `relish manual` chapter that covers it, e.g. `operations`.
    pub manual: &'static str,
}

impl CheckKind {
    /// The published manual page and section for this check.
    pub fn docs_url(&self) -> String {
        format!("{MANUAL_URL}{}", self.docs)
    }
}

/// Every check, in the order `wtf` runs them.
pub const CHECKS: &[CheckKind] = &[
    CheckKind {
        id: "collection",
        findings: &[],
        sources: &["collection"],
        tests: "Whether relish could reach the node it's connected to and collect any \
                evidence at all. With `--watch`, a failed round shows as this row.",
        docs: "07_diagnostics.md#when-the-cli-cant-reach-anything",
        manual: "diagnostics",
    },
    CheckKind {
        id: "nodes",
        findings: &["unreachable-nodes", "node-dead"],
        sources: &["nodes"],
        tests: "Reads the cluster's member list and asks every member's Bun API for its \
                health through the node you're connected to. A member that doesn't answer \
                is critical, and so is one that Mustard gossip marks dead or suspect.",
        docs: "02_cluster-basics.md#watch-it",
        manual: "cluster-basics",
    },
    CheckKind {
        id: "builds",
        findings: &["version-skew"],
        sources: &["builds"],
        tests: "Asks every node which bun it runs: version, commit and the binary's \
                SHA-256. Nodes match when version and commit match (the SHA-256 decides \
                only when the commit is unknown); any difference is a warning. A node that \
                doesn't answer makes the row UNKNOWN rather than OK.",
        docs: "12_operations.md#upgrading-bun",
        manual: "operations",
    },
    CheckKind {
        id: "os",
        findings: &["os-update-failed", "os-skew"],
        sources: &[],
        tests: "Reads the appliance OS version and update state each node reports. An OS \
                update that failed during `relish os upgrade`, or appliances on different \
                OS versions, is a warning. Nodes that aren't appliances give no row.",
        docs: "17_appliance.md#updating-the-os",
        manual: "appliance",
    },
    CheckKind {
        id: "council",
        findings: &[
            "council-recovery-split",
            "council-fence-unpersisted",
            "council-fenced",
            "council-epoch-split",
            "council-voter-lagging",
            "council-multiple-leaders",
            "no-leader",
            "quorum-loss",
            "council-member-down",
        ],
        sources: &["council"],
        tests: "Reads the council membership and every node's own view of it. No leader, \
                fewer members answering than a majority (members / 2 + 1), two leaders, \
                two recovery epochs or a fenced node is critical; a missing member while \
                quorum holds, or a voter applying the log more slowly than the others, is \
                a warning. A standalone node needs no council and is OK.",
        docs: "12_operations.md#a-walk-through-a-recovery",
        manual: "operations",
    },
    CheckKind {
        id: "faults",
        findings: &["active-faults"],
        sources: &["faults"],
        tests: "Lists the Smoker faults every node reports as still active. Any active \
                fault is a warning, because what you see may be a drill.",
        docs: "05_chaos.md#one-shot-faults",
        manual: "chaos",
    },
    CheckKind {
        id: "disks",
        findings: &["disk-high"],
        sources: &["disks"],
        tests: "Reads how full each node's filesystem is for every storage domain it \
                holds (data, images, logs, metrics, volumes), from the node's diagnostics.",
        docs: "12_operations.md#sizing-a-nodes-disk",
        manual: "operations",
    },
    CheckKind {
        id: "exports",
        findings: &["export-failing"],
        sources: &["export_holds"],
        tests: "Asks every node whether its logs or metrics export to `export_path` keeps \
                failing. A node prunes only what it has exported, so a failing export puts \
                its size cap and retention on hold; each one is a warning. With no export \
                failing there's no row.",
        docs: "04_observability.md#logs",
        manual: "observability",
    },
    CheckKind {
        id: "certificates",
        findings: &["cert-expired", "cert-expiring"],
        sources: &["certificates", "certificate_rotation"],
        tests: "Reads every node and workload certificate's expiry and rotation state. One \
                past its not-after time is critical; one expiring soon without healthy \
                automatic rotation is a warning. A consumer that can't hot-rotate its leaf \
                makes the row UNKNOWN.",
        docs: "10_security.md#between-nodes",
        manual: "security",
    },
    CheckKind {
        id: "registry",
        findings: &[
            "registry-lost-images",
            "registry-needs-mtls",
            "registry-redundancy",
        ],
        sources: &["registry"],
        tests: "Asks every node's Pickle registry whether it's listening, reaches its peers \
                and holds enough copies of each layer. An image with a layer no live node \
                holds is critical; a plaintext cluster, a registry that's down or \
                unreachable, or layers below `[images] redundancy` is a warning. On a \
                one-node cluster one copy is all there can be, so that row stays OK.",
        docs: "11_images-and-volumes.md#image-durability-and-cleanup",
        manual: "images-and-volumes",
    },
    CheckKind {
        id: "tokens",
        findings: &["token-expired", "token-expiring", "admin-token-old"],
        sources: &["tokens"],
        tests: "Reads every API token's expiry and age (only an unscoped admin may list \
                them). An expired token, one expiring soon, or an admin token that never \
                expires and is old is a warning. Tokens `relish mcp` makes and renews \
                itself are left out.",
        docs: "10_security.md#lifetimes",
        manual: "security",
    },
    CheckKind {
        id: "namespace-isolation",
        findings: &["namespace-isolation"],
        sources: &["isolation"],
        tests: "Reads the namespaces that have desired apps and whether each node's live \
                eBPF hooks and runtime can keep them apart. With two or more namespaces, a \
                node that can't is a warning; with one there's nothing to keep apart.",
        docs: "03_networking.md#who-may-connect",
        manual: "networking",
    },
    CheckKind {
        id: "gitops",
        findings: &[
            "gitops-plaintext-secret",
            "gitops-local-config",
            "gitops-config-unreadable",
        ],
        sources: &["gitops"],
        tests: "When the cluster has a GitOps configuration, asks every node how it sees \
                it. A secret-looking plaintext value in the repository, a node whose own \
                `[gitops]` differs and is ignored, or a node that can't unseal the GitOps \
                credentials is a warning. Without GitOps there's no row.",
        docs: "12_operations.md#gitops",
        manual: "operations",
    },
    CheckKind {
        id: "backups",
        findings: &["backup-disabled", "backup-not-configured", "backup-failing"],
        sources: &["backups"],
        tests: "Asks every node what its council backup loop is doing. `[cluster.backup]` \
                set but unable to run is critical; a council member without it while others \
                have it, the leader's last export failing, or no export for several \
                intervals is a warning. No member configured is OK and says so.",
        docs: "12_operations.md#backing-up-the-council",
        manual: "operations",
    },
    CheckKind {
        id: "policies",
        findings: &["policy-broken", "policy-would-deny", "policy-audit"],
        sources: &["policies"],
        tests: "Evaluates every committed app and job against the policies in force, as \
                `relish policy audit` does. A policy that can't be evaluated, or work a \
                deny or audit policy would now flag, is a warning. With no policies \
                there's no row.",
        docs: "10_security.md#auditing-running-work",
        manual: "security",
    },
    CheckKind {
        id: "crashloops",
        findings: &["crashloop"],
        sources: &["restarts", "recent_logs"],
        tests: "Counts each app's `restart` events in every node's event ring (the same \
                ones `relish events` shows) inside a recent window. Enough of them is \
                critical, with any recent deploy and the app's first error log line \
                attached. A lifetime restart count is not an input.",
        docs: "07_diagnostics.md#relish-wtf-whats-broken-and-why",
        manual: "diagnostics",
    },
    CheckKind {
        id: "services",
        findings: &["no-backends"],
        sources: &["services"],
        tests: "For every app with a service port that wants replicas, reads how many of \
                its backends are healthy. None healthy is critical.",
        docs: "01_deploy-an-app.md#health-checks",
        manual: "deploy-an-app",
    },
    CheckKind {
        id: "replicas",
        findings: &[
            "launch-failed",
            "quota-blocked",
            "volume-home-away",
            "unschedulable",
            "under-replicated",
        ],
        sources: &["replicas"],
        tests: "Compares each app's desired replicas with the instances every node reports \
                running. Fewer running is a finding that names why: the runtime refused \
                the launch or the volume's home node is away (critical), or the namespace \
                quota, the scheduler or a lost node left replicas unplaced (warning).",
        docs: "01_deploy-an-app.md#when-some-replicas-dont-fit",
        manual: "deploy-an-app",
    },
    CheckKind {
        id: "rollouts",
        findings: &["rollout-failed"],
        sources: &[],
        tests: "Reads whether an app's current spec failed to roll out on a node, which \
                holds its other nodes on the previous spec. Each one is a warning. With \
                none there's no row.",
        docs: "01_deploy-an-app.md#rolling-deploys",
        manual: "deploy-an-app",
    },
    CheckKind {
        id: "deploys",
        findings: &["deploy-stuck"],
        sources: &["deploys"],
        tests: "Reads every node's deploy operations. One still active after the stuck \
                threshold is a warning.",
        docs: "01_deploy-an-app.md#rolling-deploys",
        manual: "deploy-an-app",
    },
    CheckKind {
        id: "alerts",
        findings: &["alerts-firing"],
        sources: &["alerts"],
        tests: "Reads the Mayo alert rules every node evaluates. Each firing alert is a \
                warning.",
        docs: "04_observability.md#alerts",
        manual: "observability",
    },
    CheckKind {
        id: "cpu-throttling",
        findings: &["cpu-throttling"],
        sources: &["cpu_throttling"],
        tests: "Reads how much throttled CPU time each app's cgroup gained over the \
                observation window. Any at all is a warning: the app wants more CPU than \
                its limit.",
        docs: "07_diagnostics.md#every-finding",
        manual: "diagnostics",
    },
];

/// The check an OK row, finding id or UNKNOWN source belongs to.
pub fn check_for(id: &str) -> Option<&'static CheckKind> {
    CHECKS
        .iter()
        .find(|check| check.id == id || check.findings.contains(&id) || check.sources.contains(&id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::manual::assets::chapters;
    use crate::relish::manual::find_chapter;
    use crate::relish::wtf::findings::FINDINGS;

    #[test]
    fn every_finding_belongs_to_exactly_one_check() {
        for kind in FINDINGS {
            let owners: Vec<&str> = CHECKS
                .iter()
                .filter(|check| check.findings.contains(&kind.id))
                .map(|check| check.id)
                .collect();
            assert_eq!(owners.len(), 1, "{} belongs to {owners:?}", kind.id);
        }
        for check in CHECKS {
            for id in check.findings {
                assert!(
                    FINDINGS.iter().any(|kind| kind.id == *id),
                    "{} lists unknown finding {id}",
                    check.id
                );
            }
        }
    }

    #[test]
    fn every_check_explains_itself_and_names_a_manual_section() {
        let chapters = chapters();
        for check in CHECKS {
            assert!(check.tests.len() > 40, "{} needs an explanation", check.id);
            let index = find_chapter(&chapters, check.manual)
                .unwrap_or_else(|_| panic!("{}: no chapter {}", check.id, check.manual));
            let (file, section) = check.docs.split_once('#').expect("docs names a section");
            let chapter = chapters
                .iter()
                .find(|chapter| chapter.file == file)
                .unwrap_or_else(|| panic!("{}: no chapter file {file}", check.id));
            assert_eq!(
                chapters[index].file, file,
                "{}: `relish manual {}` opens another chapter than its docs link",
                check.id, check.manual
            );
            let headings = crate::relish::agent::manual::anchors(&chapter.markdown);
            assert!(
                headings.iter().any(|heading| heading == section),
                "{}: {file} has no heading #{section}",
                check.id
            );
        }
    }

    #[test]
    fn every_unknown_source_belongs_to_a_check() {
        let inputs = crate::relish::wtf::diagnose::tests::unknown_inputs();
        let report = crate::relish::wtf::diagnose(&inputs);
        assert!(!report.unknown.is_empty());
        for unknown in &report.unknown {
            assert!(
                check_for(&unknown.source).is_some(),
                "UNKNOWN source {} belongs to no check",
                unknown.source
            );
        }
    }

    #[test]
    fn every_ok_row_names_a_check() {
        let report =
            crate::relish::wtf::diagnose(&crate::relish::wtf::diagnose::tests::healthy_inputs());
        for ok in &report.ok {
            assert!(
                CHECKS.iter().any(|check| check.id == ok.id),
                "OK row {} is no check",
                ok.id
            );
        }
    }

    #[test]
    fn ids_and_sources_are_never_shared_between_checks() {
        let mut seen = std::collections::BTreeMap::new();
        for check in CHECKS {
            let mut names: Vec<&str> = vec![check.id];
            names.extend(check.findings);
            names.extend(check.sources);
            names.sort_unstable();
            names.dedup();
            for name in names {
                if let Some(other) = seen.insert(name, check.id) {
                    panic!("{name} is in both {other} and {}", check.id);
                }
            }
        }
    }
}
