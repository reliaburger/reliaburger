//! The live-voter guard on `relish council recover` (#694).
//!
//! Recovery starts a new council from a stopped node's state. If any voter
//! of the council that state came from still serves, that makes two
//! councils. The node being recovered is stopped, so its own agent can't
//! say whether its peers are alive; this guard asks the peers themselves.
//! It reads the voter set from the recovery source (the data directory's
//! last membership, or the backup's), and asks each voter's API whether it
//! is an unfenced council voter. A voter that answers yes, or answers in a
//! way the guard can't read, stops the recovery. A voter that doesn't
//! answer at all counts as gone.

use std::net::SocketAddr;
use std::time::Duration;

use crate::council::types::CouncilNodeInfo;

/// How long one voter gets to answer before it counts as gone.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A persisted voter and the API address the guard asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoterEndpoint {
    /// The voter's node name.
    pub name: String,
    /// Its API address: the voter's Raft IP with the cluster's API port.
    pub api_address: SocketAddr,
}

impl VoterEndpoint {
    /// The endpoint for a persisted voter, on the cluster's `api_port`.
    pub fn from_info(info: &CouncilNodeInfo, api_port: u16) -> Self {
        Self {
            name: info.name.clone(),
            api_address: SocketAddr::new(info.addr.ip(), api_port),
        }
    }
}

/// What one voter's API said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoterAnswer {
    /// Nothing answered: the voter is gone, or its API is.
    Unreachable,
    /// It answered and isn't an unfenced council voter any more.
    NotVoter,
    /// It answered as an unfenced council voter.
    Voter,
    /// Something answered, but not in a way the guard understands. The
    /// guard fails closed on this.
    Unreadable(String),
}

/// Why the guard refuses a recovery.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardRefusal {
    /// The recovery source names no voters, so there is nothing to probe.
    #[error(
        "the recovery source holds no council membership, so the voters that must be gone can't \
         be checked; pass --force once you have confirmed every voter is stopped"
    )]
    NoVoterSet,
    /// A persisted voter still serves a council.
    #[error(
        "council voter {name} ({address}) still answers as a council member; stop it or pass \
         --force"
    )]
    LiveVoter {
        /// The voter's node name.
        name: String,
        /// The address that answered.
        address: SocketAddr,
    },
    /// A persisted voter answered, but its answer couldn't be read.
    #[error(
        "council voter {name} ({address}) answered, but not as a reliaburger node this guard \
         can read ({reason}); confirm it is stopped, then pass --force"
    )]
    UnreadableVoter {
        /// The voter's node name.
        name: String,
        /// The address that answered.
        address: SocketAddr,
        /// What went wrong reading it.
        reason: String,
    },
}

/// Ask one voter's `/v1/health` whether it is an unfenced council voter,
/// over HTTPS first and then plain HTTP. Certificates aren't verified: the
/// worst a spoofed answer can do is make the guard refuse.
pub async fn probe_voter(client: &reqwest::Client, address: SocketAddr) -> VoterAnswer {
    let mut answer = VoterAnswer::Unreachable;
    for scheme in ["https", "http"] {
        let url = format!("{scheme}://{address}/v1/health");
        let response = match client.get(&url).timeout(PROBE_TIMEOUT).send().await {
            Ok(response) => response,
            // A TLS handshake against a plain listener fails while
            // connecting too, so try the other scheme before giving up.
            Err(error) if error.is_connect() || error.is_timeout() => continue,
            Err(error) => {
                answer = VoterAnswer::Unreadable(error.to_string());
                continue;
            }
        };
        return read_health(response).await;
    }
    answer
}

async fn read_health(response: reqwest::Response) -> VoterAnswer {
    let status = response.status();
    if !status.is_success() {
        return VoterAnswer::Unreadable(format!("/v1/health returned {status}"));
    }
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return VoterAnswer::Unreadable(error.to_string()),
    };
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(json) => json,
        Err(error) => return VoterAnswer::Unreadable(format!("not JSON: {error}")),
    };
    match json
        .get("council_voter")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => VoterAnswer::Voter,
        Some(false) => VoterAnswer::NotVoter,
        None => VoterAnswer::Unreadable("no council_voter field".to_string()),
    }
}

/// Probe every persisted voter at once. `Ok` means none of them still
/// serves a council; otherwise the refusal names the first that does (or
/// can't be read). An empty voter set fails closed.
pub async fn check_voters_gone(
    client: &reqwest::Client,
    voters: &[VoterEndpoint],
) -> Result<(), GuardRefusal> {
    if voters.is_empty() {
        return Err(GuardRefusal::NoVoterSet);
    }
    let probes = voters
        .iter()
        .map(|voter| async move { (voter, probe_voter(client, voter.api_address).await) });
    for (voter, answer) in futures_util::future::join_all(probes).await {
        match answer {
            VoterAnswer::Unreachable | VoterAnswer::NotVoter => {}
            VoterAnswer::Voter => {
                return Err(GuardRefusal::LiveVoter {
                    name: voter.name.clone(),
                    address: voter.api_address,
                });
            }
            VoterAnswer::Unreadable(reason) => {
                return Err(GuardRefusal::UnreadableVoter {
                    name: voter.name.clone(),
                    address: voter.api_address,
                    reason,
                });
            }
        }
    }
    Ok(())
}

/// The client the guard probes with: no certificate verification (see
/// [`probe_voter`]), and nothing else from the CLI's configuration.
pub fn probe_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(PROBE_TIMEOUT)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake voter: a plain-HTTP server answering `/v1/health` with `body`.
    async fn fake_voter(body: &'static str) -> SocketAddr {
        let app = axum::Router::new().route(
            "/v1/health",
            axum::routing::get(move || async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        address
    }

    /// An address nothing listens on.
    async fn gone() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    fn voter(name: &str, api_address: SocketAddr) -> VoterEndpoint {
        VoterEndpoint {
            name: name.to_string(),
            api_address,
        }
    }

    #[tokio::test]
    async fn recover_refuses_while_a_persisted_voter_answers() {
        let live = fake_voter(r#"{"status":"ok","council_voter":true}"#).await;
        let voters = [voter("node-1", gone().await), voter("node-2", live)];
        let refused = check_voters_gone(&probe_client().unwrap(), &voters).await;
        assert_eq!(
            refused,
            Err(GuardRefusal::LiveVoter {
                name: "node-2".to_string(),
                address: live,
            })
        );
        assert!(refused.unwrap_err().to_string().contains("node-2"));
    }

    #[tokio::test]
    async fn recover_proceeds_when_every_voter_is_gone_or_no_longer_votes() {
        let worker = fake_voter(r#"{"status":"ok","council_voter":false}"#).await;
        let voters = [voter("node-1", gone().await), voter("node-2", worker)];
        assert_eq!(
            check_voters_gone(&probe_client().unwrap(), &voters).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn recover_fails_closed_when_the_voter_set_is_unreadable() {
        // No membership to probe at all.
        assert_eq!(
            check_voters_gone(&probe_client().unwrap(), &[]).await,
            Err(GuardRefusal::NoVoterSet)
        );
        // Something answers where a voter was, but not as a node this
        // guard can read: refuse rather than guess.
        let stranger = fake_voter(r#"{"status":"ok"}"#).await;
        let refused =
            check_voters_gone(&probe_client().unwrap(), &[voter("node-1", stranger)]).await;
        assert!(
            matches!(refused, Err(GuardRefusal::UnreadableVoter { ref name, .. }) if name == "node-1"),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_voters_api_address_is_its_raft_ip_on_the_api_port() {
        let info = CouncilNodeInfo::new("10.0.0.7:9444".parse().unwrap(), "node-7");
        assert_eq!(
            VoterEndpoint::from_info(&info, 9117),
            voter("node-7", "10.0.0.7:9117".parse().unwrap())
        );
    }
}
