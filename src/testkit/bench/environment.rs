//! Reproducibility evidence collected before a benchmark run.

use std::collections::BTreeMap;

use crate::bun::capabilities::{
    ClusterCapabilities, ClusterCapabilityReport, CollectedNodeCapability,
};

use super::report::{
    BenchEnvironment, BenchNodeFingerprint, HostedFingerprint, TopologyFingerprint,
};

/// Turn local and bounded cluster capability evidence into a fingerprint.
pub fn build_environment(
    local: &ClusterCapabilities,
    cluster: &ClusterCapabilityReport,
    council_members: u32,
    hosted: HostedFingerprint,
) -> BenchEnvironment {
    let mut nodes = Vec::new();
    let mut unknown_nodes = Vec::new();
    for entry in &cluster.nodes {
        match entry {
            CollectedNodeCapability::Evidence {
                node_id, report, ..
            } => nodes.push(BenchNodeFingerprint {
                node_id: node_id.clone(),
                build: report.build.clone(),
                runtime: report.runtime.clone(),
            }),
            CollectedNodeCapability::Unknown { node_id, .. } => {
                unknown_nodes.push(node_id.clone());
            }
        }
    }
    nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    unknown_nodes.sort();

    BenchEnvironment {
        cluster_name: local.cluster_name.clone(),
        cluster_id: local.cluster_id.clone(),
        topology: TopologyFingerprint {
            node_count: local.node_count.max(cluster.nodes.len() as u32).max(1),
            council_members,
        },
        nodes,
        unknown_nodes,
        hosted,
    }
}

/// Identify common hosted runners without making an undeclared network call.
pub fn detect_hosted_environment(environment: &BTreeMap<String, String>) -> HostedFingerprint {
    let provider = if environment
        .get("GITHUB_ACTIONS")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        Some("github-actions")
    } else if environment.contains_key("GITLAB_CI") {
        Some("gitlab-ci")
    } else if environment.contains_key("BUILDKITE") {
        Some("buildkite")
    } else if environment.contains_key("CI") {
        Some("unknown-ci")
    } else {
        None
    };
    HostedFingerprint {
        detected: provider.is_some(),
        provider: provider.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::capabilities::{BuildFingerprint, CollectedNodeCapability, RuntimeFingerprint};

    fn capability(node_id: &str) -> ClusterCapabilities {
        ClusterCapabilities {
            node_id: node_id.to_string(),
            cluster_name: "bench-cluster".to_string(),
            cluster_id: Some("cluster-id".to_string()),
            node_count: 2,
            build: BuildFingerprint {
                version: "0.1.0".to_string(),
                git_sha: Some("abc".to_string()),
                target: "aarch64-apple-darwin".to_string(),
                profile: "release".to_string(),
            },
            runtime: RuntimeFingerprint {
                node_id: node_id.to_string(),
                name: "apple".to_string(),
                version: Some("1.0".to_string()),
                rootless: false,
                kernel: "Darwin 25".to_string(),
                architecture: "aarch64".to_string(),
            },
            ..ClusterCapabilities::default()
        }
    }

    #[test]
    fn cluster_fingerprint_retains_evidence_and_unknown_nodes() {
        let local = capability("node-a");
        let cluster = ClusterCapabilityReport {
            schema_version: 3,
            collected_by: "node-a".to_string(),
            observed_at_unix_ms: 1,
            deadline_at_unix_ms: 2,
            nodes: vec![
                CollectedNodeCapability::Evidence {
                    node_id: "node-a".to_string(),
                    address: "10.0.0.1:9117".to_string(),
                    report: Box::new(local.clone()),
                },
                CollectedNodeCapability::Unknown {
                    node_id: "node-b".to_string(),
                    address: "10.0.0.2:9117".to_string(),
                    reason: "deadline expired".to_string(),
                },
            ],
        };

        let fingerprint = build_environment(&local, &cluster, 1, HostedFingerprint::default());

        assert_eq!(fingerprint.cluster_name, "bench-cluster");
        assert_eq!(fingerprint.cluster_id.as_deref(), Some("cluster-id"));
        assert_eq!(fingerprint.topology.node_count, 2);
        assert_eq!(fingerprint.topology.council_members, 1);
        assert_eq!(fingerprint.nodes.len(), 1);
        assert_eq!(fingerprint.nodes[0].node_id, "node-a");
        assert_eq!(fingerprint.unknown_nodes, ["node-b"]);
    }

    #[test]
    fn hosted_detection_records_provider_instead_of_hiding_noise() {
        let github = BTreeMap::from([("GITHUB_ACTIONS".to_string(), "true".to_string())]);
        let local = BTreeMap::new();

        assert_eq!(
            detect_hosted_environment(&github),
            HostedFingerprint {
                detected: true,
                provider: Some("github-actions".to_string()),
            }
        );
        assert_eq!(
            detect_hosted_environment(&local),
            HostedFingerprint::default()
        );
    }
}
