//! Per-namespace secret keys: the leader creates each one (F05 I4).
//!
//! A namespace opts in with `secret_key = true` in `[namespace.X]`. Raft
//! apply has to be deterministic, so the state machine can't generate a
//! key: every replica would roll a different one. Instead the leader looks
//! for opted-in namespaces with no key on a short timer, generates the
//! keypair at generation 0, re-seals the namespace's stored values under it,
//! and proposes both as one [`RaftRequest::RotateSecretKey`]. If an apply
//! changed one of those values in the meantime, the state machine refuses
//! the entry as stale and the next tick tries again.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::bun::events::{AuditEvent, EventKind, EventSeverity, EventStore};
use crate::config::app::AppSpec;
use crate::council::{CouncilError, CouncilNode, CouncilResponse, DesiredState, RaftRequest};
use crate::meat::types::AppId;
use crate::sesame::secret::SecretError;
use crate::sesame::types::AgeKeyScope;

/// How often the leader looks for namespaces waiting for a key. The check
/// borrows the local state and writes nothing when no namespace waits.
pub const NAMESPACE_KEY_INTERVAL: Duration = Duration::from_secs(5);

/// The principal audit events name for what the cluster does by itself.
pub const NAMESPACE_KEY_PRINCIPAL: &str = "system";

/// Why a namespace's first key couldn't be created this time round.
#[derive(Debug, thiserror::Error)]
pub enum NamespaceKeyError {
    /// The Raft write failed.
    #[error(transparent)]
    Council(#[from] CouncilError),
    /// Generating the key or re-sealing a value failed.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// The leader holds no wrapping key, so it can't wrap a new private key.
    #[error("this node holds no wrapping key, so it can't create namespace {0}'s secret key")]
    NoWrappingKey(String),
    /// The blocking re-seal task panicked or was cancelled.
    #[error("re-sealing namespace {namespace}'s secrets failed: {source}")]
    Reseal {
        namespace: String,
        source: tokio::task::JoinError,
    },
}

/// Namespaces with `secret_key = true` and no key yet, in name order.
pub fn namespaces_awaiting_a_key(state: &DesiredState) -> Vec<String> {
    state
        .namespaces
        .iter()
        .filter(|(name, spec)| {
            spec.secret_key && !state.security_state.has_namespace_key(name.as_str())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Create the first key of every namespace waiting for one, if this node
/// leads. Returns the namespaces that got a key.
///
/// A follower does nothing. A namespace whose entry the state machine
/// refuses (a value changed under the leader) is skipped until next time.
pub async fn provision_namespace_keys_once(
    council: &CouncilNode,
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
) -> Result<Vec<String>, NamespaceKeyError> {
    if !council.is_leader().await {
        return Ok(Vec::new());
    }
    let waiting = council.read_desired(namespaces_awaiting_a_key).await;
    let mut created = Vec::new();
    for namespace in waiting {
        if let Some(resealed) = create_namespace_key(council, &namespace).await? {
            record(events, node, &namespace, resealed).await;
            created.push(namespace);
        }
    }
    Ok(created)
}

/// Generate, re-seal and propose one namespace's first key. `None` when the
/// state machine refused the entry; otherwise how many values it re-sealed.
async fn create_namespace_key(
    council: &CouncilNode,
    namespace: &str,
) -> Result<Option<usize>, NamespaceKeyError> {
    let Some(ikm) = council.wrapping_ikm().copied() else {
        return Err(NamespaceKeyError::NoWrappingKey(namespace.to_string()));
    };
    let (apps, security) = council
        .read_desired(|state| {
            let apps: Vec<(AppId, AppSpec)> = state
                .apps
                .iter()
                .filter(|(app_id, _)| app_id.namespace == namespace)
                .map(|(app_id, spec)| (app_id.clone(), spec.clone()))
                .collect();
            (apps, state.security_state.clone())
        })
        .await;
    let scope = AgeKeyScope::Namespace(namespace.to_string());
    let (keypair, _identity) = crate::sesame::secret::generate_age_keypair(scope.clone(), &ikm, 0)?;

    // Until the new key commits, the namespace's values open with the
    // cluster-wide keys, so those are the ones that read them for re-sealing.
    let identities = crate::sesame::secret::namespace_identities(&security, namespace, &ikm);
    let public_key = keypair.public_key.clone();
    let resealed = tokio::task::spawn_blocking(move || {
        crate::sesame::secret::reseal_namespace_values(
            apps.iter().map(|(app_id, spec)| (app_id, spec)),
            &identities,
            &public_key,
        )
    })
    .await
    .map_err(|source| NamespaceKeyError::Reseal {
        namespace: namespace.to_string(),
        source,
    })??;
    let count = resealed.len();

    match council
        .write(RaftRequest::RotateSecretKey {
            scope,
            new_keypair: keypair,
            resealed,
        })
        .await?
    {
        CouncilResponse::Refused { reason } => {
            eprintln!("bun: namespace {namespace} secret key not created yet: {reason}");
            Ok(None)
        }
        _ => Ok(Some(count)),
    }
}

/// Audit one namespace's new key as the cluster's own action.
async fn record(
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
    namespace: &str,
    resealed: usize,
) {
    let Some(events) = events else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    events.write().await.record_audit(AuditEvent {
        timestamp,
        kind: EventKind::Secret,
        severity: EventSeverity::Info,
        action: "secret.namespace_key_created".to_string(),
        principal: NAMESPACE_KEY_PRINCIPAL.to_string(),
        app: None,
        namespace: Some(namespace.to_string()),
        node: Some(node.to_string()),
        details: std::collections::BTreeMap::from([
            ("scope".to_string(), "namespace".to_string()),
            ("namespace".to_string(), namespace.to_string()),
            ("generation".to_string(), "0".to_string()),
            ("resealed".to_string(), resealed.to_string()),
        ]),
        message: format!(
            "namespace {namespace} has its own secret key; {resealed} stored value(s) re-sealed"
        ),
    });
}

/// Look for waiting namespaces every [`NAMESPACE_KEY_INTERVAL`] until
/// `shutdown`.
pub async fn run_namespace_key_loop(
    council: Arc<CouncilNode>,
    events: Option<Arc<RwLock<EventStore>>>,
    node: String,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(NAMESPACE_KEY_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        if let Err(error) = provision_namespace_keys_once(&council, events.as_ref(), &node).await {
            eprintln!("bun: namespace secret key: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::secret::{decrypt_secret, encrypt_secret, namespace_identities};

    const IKM: [u8; 32] = [7u8; 32];

    async fn leader_council() -> Arc<CouncilNode> {
        use std::collections::BTreeMap;

        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};

        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let node = CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            Some(IKM),
        )
        .await
        .unwrap();
        raft_router.register(1, node.raft().clone()).await;
        let members = BTreeMap::from([(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9444".parse().unwrap(),
                name: "node-1".into(),
            },
        )]);
        node.initialize(members).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !node.is_leader().await {
            assert!(tokio::time::Instant::now() < deadline, "no leader");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Arc::new(node)
    }

    fn namespace(secret_key: bool) -> crate::config::NamespaceSpec {
        crate::config::NamespaceSpec {
            cpu: None,
            memory: None,
            gpu: None,
            max_apps: None,
            max_replicas: None,
            secret_key,
        }
    }

    fn app_with_secret(value: &str, namespace: &str) -> AppSpec {
        toml::from_str(&format!(
            "image = \"web:v1\"\nnamespace = \"{namespace}\"\n[env]\nDB_PASSWORD = \"{value}\"\n"
        ))
        .unwrap()
    }

    /// A cluster with its cluster-wide key, team-a opted in, team-b not, and
    /// one app with a cluster-sealed value in each.
    async fn two_tenants(council: &CouncilNode) -> String {
        let (cluster, _) =
            crate::sesame::secret::generate_age_keypair(AgeKeyScope::ClusterWide, &IKM, 0).unwrap();
        let sealed = encrypt_secret("db-password", &cluster.public_key).unwrap();
        let security = crate::sesame::types::SecurityState {
            age_keypairs: vec![cluster],
            ..Default::default()
        };
        let writes = [
            RaftRequest::SecurityStateInit(Box::new(security)),
            RaftRequest::NamespaceSpec {
                name: "team-a".into(),
                spec: Box::new(namespace(true)),
            },
            RaftRequest::NamespaceSpec {
                name: "team-b".into(),
                spec: Box::new(namespace(false)),
            },
            RaftRequest::AppSpec {
                app_id: AppId::new("web", "team-a"),
                spec: Box::new(app_with_secret(&sealed, "team-a")),
            },
            RaftRequest::AppSpec {
                app_id: AppId::new("api", "team-b"),
                spec: Box::new(app_with_secret(&sealed, "team-b")),
            },
        ];
        for write in writes {
            council.write(write).await.unwrap();
        }
        sealed
    }

    #[tokio::test]
    async fn the_leader_creates_an_opted_in_namespaces_key_and_reseals_its_values() {
        let council = leader_council().await;
        let sealed = two_tenants(&council).await;
        let events = Arc::new(RwLock::new(EventStore::new()));

        let created = provision_namespace_keys_once(&council, Some(&events), "node-1")
            .await
            .unwrap();

        assert_eq!(created, ["team-a"]);
        let state = council.desired_state().await;
        let security = &state.security_state;
        assert!(security.has_namespace_key("team-a"));
        assert!(!security.has_namespace_key("team-b"));
        let team_a_key = security.namespace_age_keypair("team-a").unwrap();
        assert_eq!(team_a_key.generation, 0);

        let web = state.apps[&AppId::new("web", "team-a")].env["DB_PASSWORD"].as_str();
        assert_ne!(web, sealed, "team-a's value was re-sealed");
        let team_a_ids = namespace_identities(security, "team-a", &IKM);
        assert_eq!(
            team_a_ids
                .iter()
                .find_map(|id| decrypt_secret(web, id).ok())
                .as_deref(),
            Some("db-password")
        );
        assert!(
            team_a_ids
                .iter()
                .all(|id| decrypt_secret(&sealed, id).is_err()),
            "the old cluster-sealed copy no longer opens in team-a"
        );
        assert_eq!(
            state.apps[&AppId::new("api", "team-b")].env["DB_PASSWORD"].as_str(),
            sealed,
            "team-b didn't opt in"
        );

        let audit = events.read().await.recent(10, None, None);
        assert_eq!(audit.len(), 1, "{audit:?}");
        assert_eq!(
            audit[0].action.as_deref(),
            Some("secret.namespace_key_created")
        );
        assert_eq!(audit[0].principal.as_deref(), Some(NAMESPACE_KEY_PRINCIPAL));
        assert_eq!(audit[0].namespace.as_deref(), Some("team-a"));
        assert_eq!(
            audit[0].details.get("resealed").map(String::as_str),
            Some("1")
        );
        assert!(!format!("{audit:?}").contains("db-password"));
    }

    #[tokio::test]
    async fn a_namespace_with_its_key_is_left_alone() {
        let council = leader_council().await;
        two_tenants(&council).await;
        provision_namespace_keys_once(&council, None, "node-1")
            .await
            .unwrap();
        let key = council
            .security_state()
            .await
            .namespace_age_keypair("team-a")
            .cloned();
        let before = council.metrics().borrow().last_applied;

        let created = provision_namespace_keys_once(&council, None, "node-1")
            .await
            .unwrap();

        assert!(created.is_empty());
        assert_eq!(council.metrics().borrow().last_applied, before);
        assert_eq!(
            council
                .security_state()
                .await
                .namespace_age_keypair("team-a")
                .cloned(),
            key
        );
    }

    #[test]
    fn only_opted_in_namespaces_without_a_key_wait() {
        let mut state = DesiredState::default();
        for (name, opted_in) in [("a", true), ("b", false), ("c", true)] {
            state.namespaces.insert(name.into(), namespace(opted_in));
        }
        let (keypair, _) = crate::sesame::secret::generate_age_keypair(
            AgeKeyScope::Namespace("c".into()),
            &IKM,
            0,
        )
        .unwrap();
        state.security_state.age_keypairs.push(keypair);

        assert_eq!(namespaces_awaiting_a_key(&state), ["a"]);
    }
}
