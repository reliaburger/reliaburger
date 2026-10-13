/// Meat scheduler: the four-phase placement pipeline.
///
/// Filter → Score → Select → Commit. For each replica, the pipeline
/// runs iteratively: after placing one replica, the cluster state
/// cache is updated to reflect the reserved resources before placing
/// the next. This prevents over-committing a single node.
use std::collections::{BTreeMap, HashMap};

use crate::config::app::AppSpec;
use crate::config::types::Replicas;

use super::cluster_state::ClusterStateCache;
use super::filter::{WorkloadRequirements, filter_nodes, unplaced_reason};
use super::score::score_nodes;
use super::types::{AppId, Placement, Resources, SchedulingDecision, UnplacedReason};

/// Scheduling errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleError {
    #[error("no eligible nodes for app {app_id:?}")]
    NoEligibleNodes { app_id: AppId },

    #[error("quota exceeded for namespace {namespace:?}: {detail}")]
    QuotaExceeded { namespace: String, detail: String },

    #[error("invalid spec: {reason}")]
    InvalidSpec { reason: String },

    #[error("image {image} requires a signature (require_signatures is enabled)")]
    UnsignedImage { image: String },

    #[error("image {image} has an invalid signature: {reason}")]
    InvalidSignature { image: String, reason: String },
}

/// What placing an app came to: the replicas that found a node, and, when
/// some didn't, how many and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// One placement per replica that found a node, reserved in the
    /// scheduler's cache.
    pub decision: SchedulingDecision,
    /// The replicas that found no node. `None` when every one did.
    pub unplaced: Option<Unplaced>,
}

/// Replicas the scheduler couldn't place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unplaced {
    /// How many replicas have no node.
    pub missing: u32,
    /// The first hard constraint no node met.
    pub reason: UnplacedReason,
}

/// The Meat scheduler.
pub struct Scheduler {
    /// Mutable cluster state, updated after each placement.
    pub cluster: ClusterStateCache,
    /// Whether this cluster requires every placement to have live internal
    /// DNS. This comes from the leader's node configuration, not from the
    /// presence of a fresh capability lease: losing every lease must fail
    /// closed rather than silently switching DNS mode off.
    require_dns: bool,
    /// The footprint each node's committed copy of the daemon set being
    /// placed was admitted with. That copy is replaced, not doubled, so its
    /// room counts towards the copy that replaces it (#433).
    daemon_credit: HashMap<super::types::NodeId, Resources>,
}

/// The nodes a daemon set can run on, crediting back each node's committed
/// copy at the footprint it was admitted with (`credit`), so a running copy
/// never needs room for a second one. Only this eligibility view subtracts
/// it: the real cache keeps every running allocation, so other applications
/// can't spend that room. A copy the node runs without a committed placement
/// (one on its way out) earns no credit.
pub(crate) fn daemon_candidates(
    cluster: &ClusterStateCache,
    app_id: &AppId,
    credit: &HashMap<super::types::NodeId, Resources>,
    requirements: &WorkloadRequirements,
) -> Vec<super::types::NodeId> {
    let mut available = cluster.clone();
    for node in cluster.nodes() {
        if node.replicas_of(app_id) > 0
            && let Some(committed) = credit.get(&node.node_id)
        {
            available.release(&node.node_id, app_id, committed);
        }
    }
    filter_nodes(requirements, &available)
}

impl Scheduler {
    /// Create a new scheduler with the given cluster state.
    pub fn new(cluster: ClusterStateCache) -> Self {
        Self {
            cluster,
            require_dns: false,
            daemon_credit: HashMap::new(),
        }
    }

    /// Credit each node's committed copy of the daemon set being placed,
    /// at the footprint it was admitted with (see [`daemon_candidates`]).
    pub fn with_daemon_credit(mut self, credit: HashMap<super::types::NodeId, Resources>) -> Self {
        self.daemon_credit = credit;
        self
    }

    /// Require live, workload-reachable internal DNS for every placement.
    pub fn with_dns_required(mut self, required: bool) -> Self {
        self.require_dns = required;
        self
    }

    /// Schedule an app according to its spec, all or nothing.
    ///
    /// Returns a `SchedulingDecision` with one `Placement` per replica, or
    /// `NoEligibleNodes` when any replica finds no node. For daemon mode,
    /// places one replica on every eligible node, and fails when there is
    /// none. Capacity admission asks this; the leader's pass uses
    /// [`Scheduler::place_app`], which keeps what fits.
    pub fn schedule_app(
        &mut self,
        app_id: &AppId,
        spec: &AppSpec,
    ) -> Result<SchedulingDecision, ScheduleError> {
        let placed = self.place_app(app_id, spec);
        let daemon_without_a_node =
            spec.replicas == Replicas::DaemonSet && placed.decision.placements.is_empty();
        if placed.unplaced.is_some() || daemon_without_a_node {
            return Err(ScheduleError::NoEligibleNodes {
                app_id: app_id.clone(),
            });
        }
        Ok(placed.decision)
    }

    /// Place as many of an app's replicas as fit (#702).
    ///
    /// A fixed-size app gets every replica that finds a node, each reserved
    /// in the cache before the next is placed, and `unplaced` says how many
    /// found none and why. A daemon set gets one replica on every eligible
    /// node, which may be none; it is never short, since it wants exactly
    /// the nodes that can run it.
    pub fn place_app(&mut self, app_id: &AppId, spec: &AppSpec) -> Placed {
        let requirements = WorkloadRequirements::for_app(spec, self.require_dns);
        let preferred = spec
            .placement
            .as_ref()
            .map(|p| parse_label_list(&p.preferred))
            .unwrap_or_default();
        let image = spec.image.as_deref();

        match spec.replicas {
            Replicas::Fixed(n) => self.schedule_fixed(app_id, n, &requirements, &preferred, image),
            Replicas::DaemonSet => self.schedule_daemon(app_id, &requirements),
        }
    }

    /// Schedule N fixed replicas, keeping every one that finds a node.
    fn schedule_fixed(
        &mut self,
        app_id: &AppId,
        replica_count: u32,
        requirements: &WorkloadRequirements,
        preferred_labels: &BTreeMap<String, String>,
        image: Option<&str>,
    ) -> Placed {
        let mut placements = Vec::with_capacity(replica_count as usize);
        let mut unplaced = None;

        for ordinal in 0..replica_count {
            // Phase 1: Filter
            let candidates = filter_nodes(requirements, &self.cluster);

            // Phase 2: Score
            let scored = score_nodes(
                &candidates,
                app_id,
                &requirements.resources,
                preferred_labels,
                &self.cluster,
                image,
            );

            // Phase 3: Select (fewest replicas, highest score, deterministic
            // tiebreak). Nothing left means no later replica fits either:
            // each one only takes room.
            let Some((selected_node, _score)) = scored.first() else {
                unplaced = Some(Unplaced {
                    missing: replica_count - ordinal,
                    reason: unplaced_reason(requirements, &self.cluster),
                });
                break;
            };

            // Phase 4: Commit (reserve in the cluster state cache)
            self.cluster
                .reserve(selected_node, app_id, &requirements.resources);

            placements.push(Placement {
                node_id: selected_node.clone(),
                resources: requirements.resources,
                ordinal,
            });
        }

        Placed {
            decision: SchedulingDecision {
                app_id: app_id.clone(),
                placements,
            },
            unplaced,
        }
    }

    /// Schedule daemon mode: one replica on every eligible node.
    fn schedule_daemon(&mut self, app_id: &AppId, requirements: &WorkloadRequirements) -> Placed {
        let candidates =
            daemon_candidates(&self.cluster, app_id, &self.daemon_credit, requirements);

        let mut placements = Vec::with_capacity(candidates.len());
        for (ordinal, node_id) in (0u32..).zip(&candidates) {
            // A credited copy is replaced: trade its footprint for the new one.
            let running = self
                .cluster
                .get_node(node_id)
                .is_some_and(|node| node.replicas_of(app_id) > 0);
            if running && let Some(committed) = self.daemon_credit.get(node_id) {
                self.cluster.release(node_id, app_id, committed);
            }
            self.cluster
                .reserve(node_id, app_id, &requirements.resources);
            placements.push(Placement {
                node_id: node_id.clone(),
                resources: requirements.resources,
                ordinal,
            });
        }

        Placed {
            decision: SchedulingDecision {
                app_id: app_id.clone(),
                placements,
            },
            unplaced: None,
        }
    }
}

/// Parse a list of "key=value" strings into a BTreeMap.
pub(crate) fn parse_label_list(labels: &[String]) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter_map(|s| {
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Image signature enforcement
// ---------------------------------------------------------------------------

/// Check whether an image is schedulable under the trust policy.
///
/// If `require_signatures` is `true` and the image is in the Pickle
/// manifest catalog without a signature, returns `UnsignedImage`.
/// External images (not in catalog) are allowed regardless.
pub fn check_image_schedulable(
    image: Option<&str>,
    catalog: &crate::pickle::types::ManifestCatalog,
    require_signatures: bool,
) -> Result<(), ScheduleError> {
    if !require_signatures {
        return Ok(());
    }

    let Some(image_ref) = image else {
        return Ok(()); // process workloads have no image
    };

    // Look up in catalog — if not found, it's an external image (allow it)
    if let Some(manifest) = lookup_pickle_manifest(image_ref, catalog)
        && manifest.signature.is_none()
    {
        return Err(ScheduleError::UnsignedImage {
            image: image_ref.to_string(),
        });
    }

    Ok(())
}

/// Split an image name into `(name, tag)`, defaulting the tag to
/// `latest`. Only a colon *after* the last `/` starts a tag, so a
/// registry port (`localhost:5050/myapp`) is not mistaken for one.
pub(crate) fn split_repo_tag(name: &str) -> (&str, &str) {
    let tail_start = name.rfind('/').map(|i| i + 1).unwrap_or(0);
    match name[tail_start..].rfind(':') {
        Some(i) => (&name[..tail_start + i], &name[tail_start + i + 1..]),
        None => (name, "latest"),
    }
}

/// Canonical repository path for trust-policy lookups: strip a leading
/// registry host (a first segment containing `.` or `:`, or exactly
/// `localhost`) and keep the *whole* remaining path.
///
/// Basename stripping was the IMG1 bug: `team/app` collapsed to `app`
/// and missed its own policy, while an unrelated `docker.io/library/
/// app` matched a check meant for the local `app`.
pub(crate) fn canonical_repository(name: &str) -> &str {
    match name.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            rest
        }
        _ => name,
    }
}

/// Look up a Pickle-hosted manifest for an image reference.
///
/// A digest-pinned reference (`repo@sha256:…`) resolves content-
/// addressed; otherwise the canonical repository path (see
/// [`canonical_repository`]) and tag are looked up. Returns `None`
/// for images not in the catalogue — i.e. external-registry images.
///
/// Pull-through-cached upstream images live under `cache/<host>/
/// <repo>` and are exempt by construction: unsigned upstream content
/// stays deployable under `require_signatures` (it was never signable
/// by us). Pinned by `check_image_schedulable_exempts_pull_through_cache`.
/// Upstream images are bound to digests at apply and checked against the
/// node's `[[images.trust_policy.upstream]]` rules (`pickle::trust`).
/// TODO(#361): cosign signatures on upstream images (F03 U3).
pub(crate) fn lookup_pickle_manifest<'a>(
    image_ref: &str,
    catalog: &'a crate::pickle::types::ManifestCatalog,
) -> Option<&'a crate::pickle::types::ImageManifest> {
    let manifest = match image_ref.split_once('@') {
        Some((name, digest)) => {
            // `app:v1@sha256:…` names repository `app`: the digest wins and
            // the tag is only for people, but left on it would miss the
            // catalogue and pass as an external image.
            let (repository, _tag) = split_repo_tag(name);
            catalog.get_repository_manifest(canonical_repository(repository), digest)
        }
        None => {
            let (name, tag) = split_repo_tag(image_ref);
            catalog.get_manifest_by_tag(canonical_repository(name), tag)
        }
    }?;
    (!manifest.repository.starts_with("cache/")).then_some(manifest)
}

/// Pin an image reference to a verified manifest digest, producing the
/// immutable `name@sha256:…` form the runtime should pull. A reference
/// that is already pinned is returned unchanged.
pub fn pin_image_reference(image_ref: &str, digest: &crate::pickle::types::Digest) -> String {
    if image_ref.contains('@') {
        return image_ref.to_string();
    }
    let (name, _tag) = split_repo_tag(image_ref);
    format!("{name}@{}", digest.as_str())
}

/// Verify an image's signature under the trust policy, for enforcement.
///
/// Unlike [`check_image_schedulable`] (presence only), this actually verifies
/// the signature bytes. Semantics:
/// - `require_signatures == false` → allowed, nothing to pin (`Ok(None)`);
/// - no image (process workload) → allowed (`Ok(None)`);
/// - external image (not in the Pickle catalogue) → allowed (out of scope of
///   cluster trust, `Ok(None)`);
/// - Pickle image with no signature → [`ScheduleError::UnsignedImage`];
/// - Pickle image with a signature → verified via
///   [`crate::pickle::signing::verify_signature`], mapping failure to
///   [`ScheduleError::InvalidSignature`].
///
/// On success the *verified manifest digest* comes back (`Ok(Some(digest))`).
/// The caller must deploy that digest-pinned identity (see
/// [`pin_image_reference`]) rather than the tag it verified: a tag can move
/// between verification and pull, and the signature only covers the digest
/// (IMG1).
pub fn verify_image_signature(
    image: Option<&str>,
    catalog: &crate::pickle::types::ManifestCatalog,
    trust_policy: &crate::config::node::TrustPolicySection,
    trusted_roots: &[Vec<u8>],
    crl: Option<&crate::sesame::types::Crl>,
) -> Result<Option<crate::pickle::types::Digest>, ScheduleError> {
    if !trust_policy.require_signatures {
        return Ok(None);
    }

    let Some(image_ref) = image else {
        return Ok(None);
    };

    let Some(manifest) = lookup_pickle_manifest(image_ref, catalog) else {
        return Ok(None); // external image
    };

    let Some(signature) = &manifest.signature else {
        return Err(ScheduleError::UnsignedImage {
            image: image_ref.to_string(),
        });
    };

    crate::pickle::signing::verify_signature(
        signature,
        &manifest.digest,
        trust_policy,
        trusted_roots,
        crl,
    )
    .map_err(|e| ScheduleError::InvalidSignature {
        image: image_ref.to_string(),
        reason: e.to_string(),
    })?;

    Ok(Some(manifest.digest.clone()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::config::app::PlacementSpec;
    use crate::config::types::ResourceRange;
    use crate::meat::cluster_state::SchedulerNodeState;
    use crate::meat::types::NodeId;

    fn default_spec() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn node_state(
        name: &str,
        cpu: u64,
        mem: u64,
        labels: BTreeMap<String, String>,
    ) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, mem, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
            uptime_secs: 86400,
            cached_images: HashSet::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn cluster_3_nodes() -> ClusterStateCache {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("n1", 2000, 4096, BTreeMap::new()));
        cluster.set_node(node_state("n2", 2000, 4096, BTreeMap::new()));
        cluster.set_node(node_state("n3", 2000, 4096, BTreeMap::new()));
        cluster
    }

    #[test]
    fn schedule_fixed_replicas_places_all() {
        let mut scheduler = Scheduler::new(cluster_3_nodes());

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(3);
        spec.cpu = Some(ResourceRange {
            request: 500,
            limit: 1000,
        });
        spec.memory = Some(ResourceRange {
            request: 1024,
            limit: 2048,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3);
        // Resources are reserved correctly
        for p in &decision.placements {
            assert_eq!(p.resources.cpu_millicores, 500);
        }

        // H8 regression: this test used to pass with all three replicas
        // on ONE node — it never asserted distinctness, and the old
        // spread weight (10 vs bin-pack's 50) stacked them.
        let distinct: std::collections::HashSet<_> =
            decision.placements.iter().map(|p| &p.node_id).collect();
        assert_eq!(
            distinct.len(),
            3,
            "replicas must spread across distinct nodes; got {:?}",
            decision.placements
        );
    }

    #[test]
    fn egress_allowlist_only_uses_nodes_with_live_dual_stack_pre_start_hooks() {
        let mut cluster = ClusterStateCache::new();
        let incapable = node_state("plain", 2000, 4096, BTreeMap::new());
        let mut capable = node_state("guarded", 2000, 4096, BTreeMap::new());
        capable.capabilities.egress = crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        };
        cluster.set_node(incapable);
        cluster.set_node(capable);

        let mut spec = default_spec();
        spec.egress = Some(crate::config::app::EgressSpec {
            allow: vec!["192.0.2.10:443".to_string()],
            allow_franchise: Vec::new(),
        });

        let decision = Scheduler::new(cluster)
            .schedule_app(&AppId::new("payer", "prod"), &spec)
            .expect("the capable node should remain eligible");

        assert_eq!(decision.placements[0].node_id, NodeId::new("guarded"));
    }

    #[test]
    fn allow_from_only_uses_nodes_that_enforce_namespace_isolation() {
        let mut cluster = ClusterStateCache::new();
        let incapable = node_state("plain", 2000, 4096, BTreeMap::new());
        let mut capable = node_state("isolating", 2000, 4096, BTreeMap::new());
        capable.capabilities.egress = crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        };
        cluster.set_node(incapable);
        cluster.set_node(capable);

        let mut spec = default_spec();
        spec.firewall = Some(crate::config::app::FirewallSpec {
            allow_from: vec!["shop/api".to_string()],
        });

        let decision = Scheduler::new(cluster)
            .schedule_app(&AppId::new("db", "storage"), &spec)
            .expect("the isolating node should remain eligible");

        assert_eq!(decision.placements[0].node_id, NodeId::new("isolating"));
    }

    #[test]
    fn dns_enabled_cluster_only_uses_nodes_with_reachable_ready_resolver() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("plain", 2000, 4096, BTreeMap::new()));
        let mut capable = node_state("dns-ready", 2000, 4096, BTreeMap::new());
        capable.capabilities.dns = crate::onion::dns::DnsCapability {
            enabled: true,
            ready: true,
            ipv4: true,
            ipv6: false,
            workload_reachable: true,
        };
        cluster.set_node(capable);

        let decision = Scheduler::new(cluster)
            .with_dns_required(true)
            .schedule_app(&AppId::new("client", "prod"), &default_spec())
            .expect("the DNS-ready node should remain eligible");

        assert_eq!(decision.placements[0].node_id, NodeId::new("dns-ready"));
    }

    /// H8: three equal nodes, three replicas — one replica per node.
    #[test]
    fn replicas_spread_across_distinct_nodes() {
        let mut scheduler = Scheduler::new(cluster_3_nodes());

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(3);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });

        let app = AppId::new("api", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        let distinct: std::collections::HashSet<_> =
            decision.placements.iter().map(|p| &p.node_id).collect();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn schedule_spreads_when_resources_tight() {
        // When nodes are already near capacity, spread scoring forces
        // distribution because bin-packing can't differentiate further.
        let mut cluster = ClusterStateCache::new();
        for i in 0..3 {
            let mut n = node_state(&format!("n{i}"), 1000, 4096, BTreeMap::new());
            n.allocated = Resources::new(800, 0, 0); // 80% utilised
            cluster.set_node(n);
        }

        let mut scheduler = Scheduler::new(cluster);
        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(3);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3);
        let nodes: HashSet<_> = decision.placements.iter().map(|p| &p.node_id).collect();
        assert_eq!(nodes.len(), 3, "tight resources should spread across nodes");
    }

    #[test]
    fn schedule_daemon_mode_on_all_nodes() {
        let mut scheduler = Scheduler::new(cluster_3_nodes());

        let mut spec = default_spec();
        spec.replicas = Replicas::DaemonSet;
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });

        let app = AppId::new("monitor", "system");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3, "daemon on all 3 nodes");
    }

    #[test]
    fn schedule_with_required_labels() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("gpu-1", 2000, 4096, labels(&[("gpu", "a100")])));
        cluster.set_node(node_state("gpu-2", 2000, 4096, labels(&[("gpu", "a100")])));
        cluster.set_node(node_state("cpu-1", 2000, 4096, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(2);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });
        spec.placement = Some(PlacementSpec {
            required: vec!["gpu=a100".to_string()],
            preferred: vec![],
        });

        let app = AppId::new("ml", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 2);
        for p in &decision.placements {
            assert!(
                p.node_id == NodeId::new("gpu-1") || p.node_id == NodeId::new("gpu-2"),
                "should only place on gpu nodes, got {:?}",
                p.node_id
            );
        }
    }

    #[test]
    fn schedule_with_preferred_labels_fallback() {
        let mut cluster = ClusterStateCache::new();
        // Only one node has the preferred label
        cluster.set_node(node_state(
            "east",
            2000,
            4096,
            labels(&[("zone", "us-east")]),
        ));
        cluster.set_node(node_state(
            "west",
            2000,
            4096,
            labels(&[("zone", "us-west")]),
        ));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(2);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });
        spec.placement = Some(PlacementSpec {
            required: vec![],
            preferred: vec!["zone=us-east".to_string()],
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        // Both should be placed (preferred is soft, not hard)
        assert_eq!(decision.placements.len(), 2);

        // First placement should prefer "east"
        assert_eq!(decision.placements[0].node_id, NodeId::new("east"));
    }

    #[test]
    fn schedule_fails_no_eligible_nodes() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("tiny", 100, 256, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.cpu = Some(ResourceRange {
            request: 9999,
            limit: 9999,
        });

        let app = AppId::new("huge", "prod");
        let result = scheduler.schedule_app(&app, &spec);
        assert!(matches!(result, Err(ScheduleError::NoEligibleNodes { .. })));
    }

    #[test]
    fn schedule_bin_packs_onto_fuller_node() {
        let mut cluster = ClusterStateCache::new();
        // "full" already has 800m allocated — will be preferred for packing
        let mut full = node_state("full", 2000, 4096, BTreeMap::new());
        full.allocated = Resources::new(800, 0, 0);
        cluster.set_node(full);
        cluster.set_node(node_state("empty", 2000, 4096, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.cpu = Some(ResourceRange {
            request: 200,
            limit: 400,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(
            decision.placements[0].node_id,
            NodeId::new("full"),
            "should bin-pack onto the fuller node"
        );
    }

    #[test]
    fn schedule_gpu_workload() {
        let mut cluster = ClusterStateCache::new();
        let mut gpu_node = node_state("gpu-1", 2000, 4096, BTreeMap::new());
        gpu_node.allocatable = Resources::new(2000, 4096, 2); // 2 GPUs
        cluster.set_node(gpu_node);
        cluster.set_node(node_state("cpu-1", 2000, 4096, BTreeMap::new())); // 0 GPUs

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.gpu = Some(1);

        let app = AppId::new("ml", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(
            decision.placements[0].node_id,
            NodeId::new("gpu-1"),
            "should place on the GPU node"
        );
    }

    #[test]
    fn parse_label_list_works() {
        let labels = vec![
            "zone=us-east".to_string(),
            "ssd=true".to_string(),
            "malformed".to_string(), // no = sign, skipped
        ];
        let map = parse_label_list(&labels);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("zone").unwrap(), "us-east");
        assert_eq!(map.get("ssd").unwrap(), "true");
    }

    // --- Image signature enforcement ---

    #[test]
    fn check_image_schedulable_allows_unsigned_when_disabled() {
        let catalog = crate::pickle::types::ManifestCatalog::default();
        let result = super::check_image_schedulable(Some("myapp:v1"), &catalog, false);
        assert!(result.is_ok());
    }

    #[test]
    fn check_image_schedulable_rejects_unsigned_when_required() {
        use crate::pickle::types::*;
        use std::collections::BTreeSet;

        let mut catalog = ManifestCatalog::default();
        let manifest = ImageManifest {
            digest: Digest::from_sha256_hex(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000001",
                ),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: "myapp".to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None, // unsigned!
        };
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "v1".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let result = super::check_image_schedulable(Some("myapp:v1"), &catalog, true);
        assert!(matches!(result, Err(ScheduleError::UnsignedImage { .. })));
    }

    /// Phase 12 D2: pull-through-cached upstream images (repository
    /// `cache/<host>/<repo>`) are unsigned upstream content and must
    /// pass `require_signatures`. The exemption holds by construction:
    /// `lookup_pickle_manifest` refuses to match any `cache/...`
    /// repository — this test pins that behaviour so a future lookup
    /// change can't silently start refusing every cached external
    /// image.
    #[test]
    fn check_image_schedulable_exempts_pull_through_cache() {
        use crate::pickle::types::*;
        use std::collections::BTreeSet;

        let mut catalog = ManifestCatalog::default();
        let manifest = ImageManifest {
            digest: Digest::from_sha256_hex(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000001",
                ),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: "cache/docker.io/library/redis".to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None, // unsigned upstream content
        };
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "7".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        // The app spec references the upstream name, not the cache repo.
        let result = super::check_image_schedulable(Some("redis:7"), &catalog, true);
        assert!(
            result.is_ok(),
            "cached upstream images must stay deployable"
        );
    }

    #[test]
    fn check_image_schedulable_allows_signed_when_required() {
        use crate::pickle::types::*;
        use std::collections::BTreeSet;

        let mut catalog = ManifestCatalog::default();
        let manifest = ImageManifest {
            digest: Digest::from_sha256_hex(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000001",
                ),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: "myapp".to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: Some(ImageSignature {
                method: SigningMethod::Keyless {
                    issuer: "https://test.reliaburger.dev".to_string(),
                    identity: "spiffe://test/ns/ci/job/build".to_string(),
                },
                signature: "MEUCIQD...".to_string(),
                verification_material: VerificationMaterial::CertificateChain(vec![vec![1, 2, 3]]),
                signed_at: std::time::SystemTime::UNIX_EPOCH,
            }),
        };
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "v1".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let result = super::check_image_schedulable(Some("myapp:v1"), &catalog, true);
        assert!(result.is_ok());
    }

    #[test]
    fn check_image_schedulable_allows_external_image() {
        // Image not in catalog = external registry, always allowed
        let catalog = crate::pickle::types::ManifestCatalog::default();
        let result =
            super::check_image_schedulable(Some("docker.io/library/nginx:latest"), &catalog, true);
        assert!(result.is_ok());
    }

    // --- verify_image_signature (enforcement) ---

    use crate::config::node::TrustPolicySection;

    fn require_signatures() -> TrustPolicySection {
        TrustPolicySection {
            require_signatures: true,
            keys: vec![],
            ..Default::default()
        }
    }

    /// Build a catalogue holding a `myapp:v1` manifest with the given signature,
    /// and return `(catalog, manifest_digest)`.
    fn catalog_with(
        signature: Option<crate::pickle::types::ImageSignature>,
    ) -> crate::pickle::types::ManifestCatalog {
        use crate::pickle::types::*;
        use std::collections::BTreeSet;

        let mut catalog = ManifestCatalog::default();
        let manifest = ImageManifest {
            digest: Digest::from_sha256_hex(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000001",
                ),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: "myapp".to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature,
        };
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "v1".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog
    }

    /// Mint a real keyless signature over the catalogue's manifest digest,
    /// returning `(signature, root_ca_der)`.
    fn real_keyless_signature() -> (crate::pickle::types::ImageSignature, Vec<u8>) {
        use crate::pickle::types::Digest;
        use crate::sesame::ca;
        use crate::sesame::identity::{CertUsage, create_workload_csr, validate_and_sign_csr};
        use crate::sesame::types::{SerialNumber, SpiffeUri, WorkloadType};

        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-ikm-32-bytes-padding-here!!").unwrap();
        // A real code-signing leaf under the workload CA, matching the SPIFFE
        // identity the signature declares (IMG3 identity binding).
        let uri = SpiffeUri {
            trust_domain: "test".to_string(),
            namespace: "ci".to_string(),
            workload_type: WorkloadType::Job,
            name: "build-signer".to_string(),
        };
        let (csr_der, key_der) = create_workload_csr(&uri).unwrap();
        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::CodeSigning,
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
            std::time::SystemTime::now(),
        )
        .unwrap();
        let digest = Digest::from_sha256_hex(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        let sig = crate::pickle::signing::create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
            "reliaburger-council",
            &uri.to_uri(),
        )
        .unwrap();
        (sig, hierarchy.root.ca.certificate_der)
    }

    #[test]
    fn verify_image_signature_allows_any_image_when_signatures_not_required() {
        let catalog = catalog_with(None);
        let policy = TrustPolicySection::default();
        assert!(
            super::verify_image_signature(Some("myapp:v1"), &catalog, &policy, &[], None).is_ok()
        );
    }

    #[test]
    fn verify_image_signature_allows_an_external_image_not_in_the_catalog() {
        let catalog = crate::pickle::types::ManifestCatalog::default();
        let result = super::verify_image_signature(
            Some("nginx:latest"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn verify_image_signature_rejects_a_pickle_image_with_no_signature() {
        let catalog = catalog_with(None);
        let result = super::verify_image_signature(
            Some("myapp:v1"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert!(matches!(result, Err(ScheduleError::UnsignedImage { .. })));
    }

    #[test]
    fn verify_image_signature_accepts_a_pickle_image_with_a_valid_signature() {
        let (sig, root_ca) = real_keyless_signature();
        let catalog = catalog_with(Some(sig));
        let result = super::verify_image_signature(
            Some("myapp:v1"),
            &catalog,
            &require_signatures(),
            std::slice::from_ref(&root_ca),
            None,
        );
        assert!(result.is_ok(), "valid signature should verify: {result:?}");
    }

    #[test]
    fn verify_image_signature_rejects_a_pickle_image_with_a_tampered_signature() {
        let (mut sig, root_ca) = real_keyless_signature();
        // Corrupt the signature bytes.
        sig.signature = "AAAA".to_string();
        let catalog = catalog_with(Some(sig));
        let result = super::verify_image_signature(
            Some("myapp:v1"),
            &catalog,
            &require_signatures(),
            std::slice::from_ref(&root_ca),
            None,
        );
        assert!(matches!(
            result,
            Err(ScheduleError::InvalidSignature { .. })
        ));
    }

    /// Build a catalogue holding an unsigned manifest under `repo`.
    fn catalog_with_repo(repo: &str) -> crate::pickle::types::ManifestCatalog {
        use crate::pickle::types::*;
        use std::collections::BTreeSet;

        let mut catalog = ManifestCatalog::default();
        let manifest = ImageManifest {
            digest: Digest::from_sha256_hex(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000001",
                ),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        };
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "v1".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog
    }

    /// IMG1: `team/app` must resolve to the policy for `team/app`, not
    /// be stripped to `app` and miss it.
    #[test]
    fn nested_repository_reference_hits_its_own_policy() {
        let catalog = catalog_with_repo("team/app");
        let result = super::verify_image_signature(
            Some("team/app:v1"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert!(
            matches!(result, Err(ScheduleError::UnsignedImage { .. })),
            "an unsigned nested local repository must be refused: {result:?}"
        );
    }

    /// IMG1: an unrelated external image whose basename matches a local
    /// repository must not be checked against the local one.
    #[test]
    fn external_image_does_not_alias_a_local_basename() {
        let catalog = catalog_with_repo("app");
        let result = super::verify_image_signature(
            Some("docker.io/library/app:v1"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert_eq!(
            result.unwrap(),
            None,
            "an external image is out of scope of cluster trust"
        );
    }

    /// A registry-host prefix (as docker requires for pushes) still
    /// resolves to the local repository behind it.
    #[test]
    fn registry_prefixed_local_reference_still_matches_policy() {
        let catalog = catalog_with_repo("myapp");
        let result = super::verify_image_signature(
            Some("localhost:5050/myapp:v1"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert!(matches!(result, Err(ScheduleError::UnsignedImage { .. })));
    }

    /// F03: a reference carrying a tag *and* a digest (`app:v1@sha256:…`,
    /// what binding at apply writes) is still the local repository. Looked up
    /// as repository `app:v1`, it missed, counted as external and skipped
    /// the policy.
    #[test]
    fn tag_and_digest_reference_hits_its_own_policy() {
        let catalog = catalog_with_repo("app");
        let digest = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        for image in [
            format!("app:v1@{digest}"),
            format!("localhost:5050/app:v1@{digest}"),
        ] {
            let result = super::verify_image_signature(
                Some(&image),
                &catalog,
                &require_signatures(),
                &[],
                None,
            );
            assert!(
                matches!(result, Err(ScheduleError::UnsignedImage { .. })),
                "{image}: an unsigned local image must be refused: {result:?}"
            );
        }
    }

    /// An explicit `cache/...` reference is exempt like the shorthand:
    /// upstream content was never signable by us.
    #[test]
    fn explicit_cache_repository_reference_is_exempt() {
        let catalog = catalog_with_repo("cache/docker.io/library/redis");
        let result = super::verify_image_signature(
            Some("cache/docker.io/library/redis:v1"),
            &catalog,
            &require_signatures(),
            &[],
            None,
        );
        assert_eq!(result.unwrap(), None);
    }

    /// IMG1: successful verification returns the manifest digest, and a
    /// digest-pinned reference resolves content-addressed.
    #[test]
    fn verification_returns_the_pinned_digest() {
        let (sig, root_ca) = real_keyless_signature();
        let catalog = catalog_with(Some(sig));
        let expected = crate::pickle::types::Digest::from_sha256_hex(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );

        let pinned = super::verify_image_signature(
            Some("myapp:v1"),
            &catalog,
            &require_signatures(),
            std::slice::from_ref(&root_ca),
            None,
        )
        .unwrap();
        assert_eq!(pinned, Some(expected.clone()));

        // The pinned form verifies too — re-driving a deploy from a
        // stored digest reference hits the same policy.
        let repinned = super::verify_image_signature(
            Some(&super::pin_image_reference("myapp:v1", &expected)),
            &catalog,
            &require_signatures(),
            std::slice::from_ref(&root_ca),
            None,
        )
        .unwrap();
        assert_eq!(repinned, Some(expected));
    }

    #[test]
    fn pin_image_reference_replaces_the_tag() {
        let digest = crate::pickle::types::Digest::from_sha256_hex(&"a".repeat(64));
        assert_eq!(
            super::pin_image_reference("myapp:v1", &digest),
            format!("myapp@sha256:{}", "a".repeat(64))
        );
        // Registry ports are not tags.
        assert_eq!(
            super::pin_image_reference("localhost:5050/myapp", &digest),
            format!("localhost:5050/myapp@sha256:{}", "a".repeat(64))
        );
        // Already pinned references pass through untouched.
        let pinned = format!("myapp@sha256:{}", "b".repeat(64));
        assert_eq!(super::pin_image_reference(&pinned, &digest), pinned);
    }

    #[test]
    fn verify_image_signature_rejects_a_revoked_signing_cert() {
        let (sig, root_ca) = real_keyless_signature();
        let catalog = catalog_with(Some(sig));
        // The signing leaf cert has serial 100; revoke it.
        let crl = crate::sesame::types::Crl {
            retired_nodes: Default::default(),
            entries: vec![crate::sesame::types::CrlEntry {
                serial: crate::sesame::types::SerialNumber(100),
                issuer: crate::sesame::types::CaRole::Workload,
                revoked_at: std::time::SystemTime::now(),
                reason: "key leaked".to_string(),
                expires_at: None,
            }],
            version: 1,
            updated_at: std::time::SystemTime::now(),
        };
        let result = super::verify_image_signature(
            Some("myapp:v1"),
            &catalog,
            &require_signatures(),
            std::slice::from_ref(&root_ca),
            Some(&crl),
        );
        assert!(
            matches!(result, Err(ScheduleError::InvalidSignature { .. })),
            "a revoked signing cert must be refused: {result:?}"
        );
    }
}

/// Property tests over random clusters and passes (#702): whatever the
/// cluster looks like, placement never overcommits a node, spreads as evenly
/// as eligibility allows, is deterministic, and keeps every namespace within
/// its quota.
#[cfg(test)]
mod properties {
    use std::collections::{BTreeMap, HashMap, HashSet};

    use proptest::prelude::*;

    use super::*;
    use crate::config::types::ResourceRange;
    use crate::meat::cluster_state::SchedulerNodeState;
    use crate::meat::types::NodeId;

    const MIB: u64 = 1024 * 1024;

    /// One node: allocatable CPU (millicores) and memory (MiB), and how
    /// much of each is already taken, in percent.
    #[derive(Debug, Clone)]
    struct NodeShape {
        cpu: u64,
        memory_mib: u64,
        used_percent: u64,
    }

    /// One app: per-replica CPU and memory requests, and its replicas.
    #[derive(Debug, Clone)]
    struct AppShape {
        cpu: u64,
        memory_mib: u64,
        replicas: u32,
    }

    fn node_shape() -> impl Strategy<Value = NodeShape> {
        (500u64..8000, 256u64..16384, 0u64..90).prop_map(|(cpu, memory_mib, used_percent)| {
            NodeShape {
                cpu,
                memory_mib,
                used_percent,
            }
        })
    }

    fn app_shape() -> impl Strategy<Value = AppShape> {
        (0u64..2000, 0u64..4096, 1u32..8).prop_map(|(cpu, memory_mib, replicas)| AppShape {
            cpu,
            memory_mib,
            replicas,
        })
    }

    fn node(index: usize, shape: &NodeShape) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(format!("n{index:02}")),
            allocatable: Resources::new(shape.cpu, shape.memory_mib * MIB, 0),
            allocated: Resources::new(
                shape.cpu * shape.used_percent / 100,
                shape.memory_mib * MIB * shape.used_percent / 100,
                0,
            ),
            labels: BTreeMap::new(),
            ready: true,
            capabilities: Default::default(),
            app_replicas: HashMap::new(),
            uptime_secs: 86400,
            cached_images: HashSet::new(),
        }
    }

    /// The cluster, with nodes inserted in `order`: a plan mustn't depend
    /// on the order a HashMap happens to iterate in.
    fn cluster(shapes: &[NodeShape], order: &[usize]) -> ClusterStateCache {
        let mut cache = ClusterStateCache::new();
        for &index in order {
            cache.set_node(node(index, &shapes[index]));
        }
        cache
    }

    fn spec(shape: &AppShape) -> AppSpec {
        let mut spec: AppSpec = toml::from_str(r#"image = "app:1""#).unwrap();
        spec.replicas = Replicas::Fixed(shape.replicas);
        spec.cpu = Some(ResourceRange {
            request: shape.cpu,
            limit: shape.cpu.max(1),
        });
        spec.memory = Some(ResourceRange {
            request: shape.memory_mib * MIB,
            limit: (shape.memory_mib * MIB).max(1),
        });
        spec
    }

    fn app_id(index: usize) -> AppId {
        AppId::new(format!("app{index}"), "default")
    }

    /// Place every app in turn on one scheduler, as a pass does.
    fn place_all(cache: ClusterStateCache, apps: &[AppShape]) -> Vec<Placed> {
        let mut scheduler = Scheduler::new(cache);
        apps.iter()
            .enumerate()
            .map(|(index, shape)| scheduler.place_app(&app_id(index), &spec(shape)))
            .collect()
    }

    proptest! {
        #[test]
        fn never_overcommits_a_node(
            shapes in prop::collection::vec(node_shape(), 1..16),
            apps in prop::collection::vec(app_shape(), 1..10),
        ) {
            let order: Vec<usize> = (0..shapes.len()).collect();
            let start = cluster(&shapes, &order);
            let placed = place_all(start.clone(), &apps);

            // Add up what landed on each node, independently of the
            // scheduler's own cache.
            let mut added: HashMap<NodeId, Resources> = HashMap::new();
            for placement in placed.iter().flat_map(|p| &p.decision.placements) {
                let total = added.entry(placement.node_id.clone()).or_default();
                *total = total.saturating_add(&placement.resources);
            }
            for (node_id, extra) in added {
                let node = start.get_node(&node_id).unwrap();
                let total = node.allocated.saturating_add(&extra);
                prop_assert!(
                    node.allocatable.fits(&total),
                    "{} holds {} of {}", node_id, total, node.allocatable
                );
            }
        }

        #[test]
        fn spread_is_at_most_ceil_of_replicas_over_eligible(
            nodes in 1usize..12,
            replicas in 1u32..40,
            cpu in 1u64..200,
        ) {
            // Every node has room for every replica, so all stay eligible
            // and only spread decides.
            let shape = NodeShape {
                cpu: cpu * u64::from(replicas) + 100,
                memory_mib: 1024,
                used_percent: 0,
            };
            let shapes = vec![shape; nodes];
            let order: Vec<usize> = (0..nodes).collect();
            let app = AppShape { cpu, memory_mib: 0, replicas };
            let placed = place_all(cluster(&shapes, &order), &[app]);

            let decision = &placed[0].decision;
            prop_assert_eq!(decision.placements.len(), replicas as usize);
            let mut per_node: HashMap<&NodeId, usize> = HashMap::new();
            for placement in &decision.placements {
                *per_node.entry(&placement.node_id).or_default() += 1;
            }
            let ceiling = (replicas as usize).div_ceil(nodes);
            prop_assert!(
                per_node.values().all(|count| *count <= ceiling),
                "{:?} exceeds {}", per_node, ceiling
            );
        }

        #[test]
        fn same_inputs_give_the_same_plan(
            shapes in prop::collection::vec(node_shape(), 1..16),
            apps in prop::collection::vec(app_shape(), 1..10),
            seed in any::<u64>(),
        ) {
            let forward: Vec<usize> = (0..shapes.len()).collect();
            let mut shuffled = forward.clone();
            // A cheap deterministic shuffle: rotate by the seed, then reverse.
            shuffled.rotate_left((seed as usize) % shapes.len());
            shuffled.reverse();

            let first = place_all(cluster(&shapes, &forward), &apps);
            let second = place_all(cluster(&shapes, &shuffled), &apps);

            prop_assert_eq!(first, second);
        }

        #[test]
        fn quota_never_exceeded(
            shapes in prop::collection::vec(node_shape(), 1..8),
            apps in prop::collection::vec(app_shape(), 1..10),
            budget in 0u64..10_000,
        ) {
            use crate::cluster::orchestrate::{PassMembers, plan_pass};

            let mut desired = crate::council::types::DesiredState::default();
            desired.namespaces.insert(
                "default".to_string(),
                crate::config::NamespaceSpec {
                    cpu: Some(format!("{budget}m")),
                    memory: None,
                    gpu: None,
                    max_apps: None,
                    max_replicas: None,
                    secret_key: false,
                },
            );
            for (index, shape) in apps.iter().enumerate() {
                desired.apps.insert(app_id(index), spec(shape));
            }
            let order: Vec<usize> = (0..shapes.len()).collect();
            let mut cache = cluster(&shapes, &order);
            let members = PassMembers {
                alive: cache.node_ids().into_iter().collect(),
                ..Default::default()
            };
            let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);

            let plan = plan_pass(&mut cache, &desired, &members, &mut quotas, false);

            // The quota charges what an app asks for, placed or not, so
            // what's placed can only be less.
            let placed: u64 = plan
                .decisions
                .iter()
                .flat_map(|decision| &decision.placements)
                .map(|placement| placement.resources.cpu_millicores)
                .sum();
            prop_assert!(placed <= budget, "placed {}m of a {}m budget", placed, budget);
        }
    }
}
