//! Which commit the cluster's nodes run, and the warning when it isn't
//! this binary's.
//!
//! An agent that reads this source to explain what a node does is only
//! right if the node runs this source. So every `relish agent source`
//! command asks each node for its build (one `/v1/version` per node, in
//! parallel, with a short timeout) and warns when one runs another commit.
//! Nothing is cached: a node upgraded a minute ago must not be reported as
//! the old build. `--offline` skips the question.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use super::REPOSITORY_URL;
use crate::relish::client::BunClient;

/// How long each request to the cluster may take. The check is a courtesy
/// on top of the answer, so it must not hold the answer up for long.
const NODE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Characters of a commit the warnings show, as `git log --oneline` does.
const SHORT_COMMIT: usize = 7;

/// What the cluster said about the commits its nodes run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClusterSource {
    /// `--offline`: the cluster wasn't asked.
    Offline,
    /// No cluster answered. Not an error: the source is still this binary's.
    Unreachable {
        /// Why, in one line.
        reason: String,
    },
    /// Each node's build, compared with this binary's.
    Checked {
        /// One entry per node the cluster lists as up.
        nodes: Vec<NodeSource>,
    },
}

/// One node's build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NodeSource {
    /// The node's id, or the endpoint for a node that didn't list itself.
    pub node: String,
    /// The version its `bun` reports, e.g. `v0.2.7`.
    pub version: Option<String>,
    /// The commit its `bun` was built from, when it knows.
    pub commit: Option<String>,
    /// Whether that commit is this binary's. `false` when either side
    /// doesn't know its commit, or the node didn't answer.
    pub matches: bool,
    /// Why the node couldn't be asked, when it couldn't.
    pub error: Option<String>,
}

/// Whether two commits are the same, allowing either to be abbreviated.
/// Unknown on either side is not a match: nothing proves it.
pub fn commits_match(relish: Option<&str>, node: Option<&str>) -> bool {
    match (relish, node) {
        (Some(relish), Some(node)) => {
            let shorter = relish.len().min(node.len());
            shorter >= SHORT_COMMIT && relish[..shorter].eq_ignore_ascii_case(&node[..shorter])
        }
        _ => false,
    }
}

/// A node's entry from what its `/v1/version` said: `(version, commit)`, or
/// why it couldn't be asked.
pub fn node_source(
    node: &str,
    relish_commit: Option<&str>,
    answer: Result<(&str, Option<&str>), String>,
) -> NodeSource {
    match answer {
        Ok((version, commit)) => NodeSource {
            node: node.to_string(),
            version: Some(version.to_string()),
            commit: commit.map(str::to_string),
            matches: commits_match(relish_commit, commit),
            error: None,
        },
        Err(error) => NodeSource {
            node: node.to_string(),
            version: None,
            commit: None,
            matches: false,
            error: Some(error),
        },
    }
}

fn short(commit: &str) -> &str {
    commit.get(..SHORT_COMMIT).unwrap_or(commit)
}

/// The warnings every `source` command carries for `cluster`: one
/// `source_mismatch` naming each node that runs a different (or unknown)
/// commit, with a link to its code, and one naming each node that couldn't
/// be asked.
pub fn warnings(relish_commit: Option<&str>, cluster: &ClusterSource) -> Vec<String> {
    let ClusterSource::Checked { nodes } = cluster else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let differing: Vec<String> = nodes
        .iter()
        .filter(|node| node.error.is_none() && !node.matches)
        .map(|node| match &node.commit {
            Some(commit) => format!(
                "{} runs {} ({REPOSITORY_URL}/tree/{commit})",
                node.node,
                short(commit)
            ),
            None => format!(
                "{} runs {} without a known commit",
                node.node,
                node.version.as_deref().unwrap_or("an unknown version")
            ),
        })
        .collect();
    if !differing.is_empty() {
        let this = relish_commit.map_or("an unknown commit".to_string(), |commit| {
            short(commit).to_string()
        });
        warnings.push(format!(
            "source_mismatch: this relish was built from {this}, but {}; this source may not be the code those nodes run",
            differing.join(", ")
        ));
    }
    let unasked: Vec<String> = nodes
        .iter()
        .filter_map(|node| {
            let error = node.error.as_ref()?;
            Some(format!("{} ({error})", node.node))
        })
        .collect();
    if !unasked.is_empty() {
        warnings.push(format!(
            "couldn't ask which commit these nodes run: {}",
            unasked.join(", ")
        ));
    }
    warnings
}

/// Ask the cluster behind `client` which commit each node runs.
///
/// The node list comes from the entry node, then each node's
/// `/v1/version` through it, in parallel. A cluster that doesn't list its
/// nodes (a standalone node, or a token that can't read the list) is asked
/// only for the entry node's build, which is public.
pub async fn check(client: &BunClient, relish_commit: Option<&str>) -> ClusterSource {
    let listed = match tokio::time::timeout(NODE_CHECK_TIMEOUT, client.nodes()).await {
        Ok(Ok(nodes)) => nodes,
        Ok(Err(_)) => Vec::new(),
        Err(_) => {
            return ClusterSource::Unreachable {
                reason: format!(
                    "{} didn't answer within {}s",
                    client.base_url(),
                    NODE_CHECK_TIMEOUT.as_secs()
                ),
            };
        }
    };
    let up: Vec<_> = listed.iter().filter(|node| !node.is_down()).collect();
    if up.is_empty() {
        return match ask(client).await {
            Ok((version, commit)) => ClusterSource::Checked {
                nodes: vec![node_source(
                    client.base_url(),
                    relish_commit,
                    Ok((version.as_str(), commit.as_deref())),
                )],
            },
            Err(reason) => ClusterSource::Unreachable { reason },
        };
    }
    let answers = futures_util::future::join_all(up.iter().map(|node| async move {
        match client.via_node(&node.node_id) {
            Ok(node_client) => ask(&node_client).await,
            Err(error) => Err(error.to_string()),
        }
    }))
    .await;
    let nodes = up
        .iter()
        .zip(answers)
        .map(|(node, answer)| {
            let answer = answer
                .as_ref()
                .map(|(version, commit)| (version.as_str(), commit.as_deref()))
                .map_err(Clone::clone);
            node_source(&node.node_id, relish_commit, answer)
        })
        .collect();
    ClusterSource::Checked { nodes }
}

/// One node's `(version, commit)`, within the timeout.
async fn ask(client: &BunClient) -> Result<(String, Option<String>), String> {
    match tokio::time::timeout(NODE_CHECK_TIMEOUT, client.version()).await {
        Ok(Ok(version)) => Ok((version.version, version.commit)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!(
            "no answer within {}s",
            NODE_CHECK_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THIS: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

    #[test]
    fn commits_match_in_full_or_abbreviated_but_never_unknown() {
        assert!(commits_match(Some(THIS), Some(THIS)));
        assert!(commits_match(Some(THIS), Some("a1b2c3d")));
        assert!(commits_match(Some("A1B2C3D4"), Some(THIS)));
        assert!(!commits_match(Some(THIS), Some("3fcb1fd")));
        assert!(
            !commits_match(Some(THIS), Some("a1b2")),
            "too short to tell"
        );
        assert!(!commits_match(None, Some(THIS)));
        assert!(!commits_match(Some(THIS), None));
        assert!(!commits_match(None, None));
    }

    #[test]
    fn matching_nodes_raise_no_warning() {
        let cluster = ClusterSource::Checked {
            nodes: vec![
                node_source("node-1", Some(THIS), Ok(("v0.2.7", Some(THIS)))),
                node_source("node-2", Some(THIS), Ok(("v0.2.7", Some("a1b2c3d")))),
            ],
        };
        assert!(cluster_nodes(&cluster).iter().all(|node| node.matches));
        assert_eq!(warnings(Some(THIS), &cluster), Vec::<String>::new());
    }

    #[test]
    fn a_different_commit_names_the_node_and_links_its_code() {
        let cluster = ClusterSource::Checked {
            nodes: vec![
                node_source("node-1", Some(THIS), Ok(("v0.2.7", Some(THIS)))),
                node_source("node-2", Some(THIS), Ok(("v0.2.6", Some("3fcb1fd9")))),
                node_source("node-3", Some(THIS), Ok(("v0.2.6", None))),
            ],
        };
        let warnings = warnings(Some(THIS), &cluster);
        assert_eq!(
            warnings,
            [
                "source_mismatch: this relish was built from a1b2c3d, but node-2 runs 3fcb1fd \
              (https://github.com/reliaburger/reliaburger/tree/3fcb1fd9), node-3 runs v0.2.6 \
              without a known commit; this source may not be the code those nodes run"
            ]
        );
        assert!(!warnings[0].contains("node-1"));
    }

    #[test]
    fn a_relish_without_a_commit_matches_no_node() {
        let cluster = ClusterSource::Checked {
            nodes: vec![node_source("node-1", None, Ok(("v0.2.7", Some(THIS))))],
        };
        let warnings = warnings(None, &cluster);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("built from an unknown commit"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_node_that_did_not_answer_is_not_called_a_mismatch() {
        let cluster = ClusterSource::Checked {
            nodes: vec![node_source(
                "node-4",
                Some(THIS),
                Err("no answer within 2s".to_string()),
            )],
        };
        assert_eq!(
            warnings(Some(THIS), &cluster),
            ["couldn't ask which commit these nodes run: node-4 (no answer within 2s)"]
        );
    }

    #[test]
    fn offline_and_unreachable_clusters_raise_no_warning() {
        assert!(warnings(Some(THIS), &ClusterSource::Offline).is_empty());
        let unreachable = ClusterSource::Unreachable {
            reason: "refused".to_string(),
        };
        assert!(warnings(Some(THIS), &unreachable).is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_reported_not_raised() {
        // Port 9 (discard) on loopback refuses at once.
        let client = BunClient::new("http://127.0.0.1:9");
        match check(&client, Some(THIS)).await {
            ClusterSource::Unreachable { reason } => assert!(!reason.is_empty()),
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    fn cluster_nodes(cluster: &ClusterSource) -> &[NodeSource] {
        match cluster {
            ClusterSource::Checked { nodes } => nodes,
            _ => &[],
        }
    }
}
