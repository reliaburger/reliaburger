# One service with more than 32 cluster backends blocks publication of the whole consumer view

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: 33-endpoint map failure reproduced; production publication traced.

### Problem

The cluster catalogue can contain more than 32 backends for one service, but consumer publication validates the complete merged inventory with a strict per-service maximum of 32. A normally configured service scaled to 33 replicas (or a daemon running on >32 nodes) therefore makes a bun reject the entire new consumer view. New endpoints, DNS and ingress updates for unrelated services included in that view can also stop publishing.

### Evidence

- [src/onion/types.rs:16](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/types.rs#L16): `MAX_BACKENDS = 32`.
- [src/onion/service_map.rs:286–355](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L286-L355): remote merge appends the entire cluster endpoint set without checking this limit.
- [src/onion/service_map.rs:72–73](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L72-L73): `from_snapshot` rejects a service with more than 32 backends.
- [src/bun/agent/consumer.rs:304–345](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L304-L345): builds the merged candidate and validates consumer ownership.
- [src/bun/agent/consumer.rs:489](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L489): reconstructs the merged map with `from_snapshot` during publication.
- [src/onion/ebpf/maps.rs:236–252](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/ebpf/maps.rs#L236-L252): kernel converter separately limits the fixed-size backend array to 32, so simply dropping validation is insufficient.

### Reproduction / limits

`evidence/state.rs` uses production `EndpointCatalog::rebuild`, `ServiceMap::with_cluster_catalog` and `ServiceMap::from_snapshot`: 33 healthy endpoints for one service, each on a distinct node.

Actual verified output:

```text
33 cluster backends: merged=33 validation=Some(InvalidSnapshot { service: "default__crowded", reason: "backend capacity exceeded" })
```
This is a verified public map/validation reproduction plus the production consumer call path, not a live 33-node cluster run. Parent review found no cluster-wide replica admission cap in configuration/scheduling. The discovery design documents a 32-backend dataplane limit and says excess backends are dropped with a warning ([docs/design/discovery-onion.md:785](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/design/discovery-onion.md#L785)); the implemented merged-view validation instead refuses the whole publication. The defect is the accepted configuration and failure scope, even if 32 remains the supported dataplane maximum.

### Suggested fix / acceptance

Ensure admitted service sizes remain publishable. Support larger cluster endpoint sets in the dataplane, or reject unsupported cluster replica counts and rollout surge sizes at admission with clear documentation; do not permit one oversized service to poison unrelated updates. Test 33 distributed endpoints, daemon deployment on >32 nodes, rolling-surge boundaries and publication of an unrelated service.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/onion/service_map.rs:69–76](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L69-L76)

```rust
            if map.entries.contains_key(&key) || map.allocated_vips.contains(&entry.vip) {
                return Err(invalid("duplicate service or virtual IP owner"));
            }
            if entry.backends.len() > MAX_BACKENDS {
                return Err(invalid("backend capacity exceeded"));
            }
            let mut backend_ids = HashSet::new();
            for backend in &entry.backends {
```

[src/onion/service_map.rs:335–350](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L335-L350)

```rust
                        port: service.port,
                        backends: service
                            .backends
                            .iter()
                            .filter(|backend| Some(backend.node_id.as_str()) != local_node)
                            .map(|b| BackendInstance {
                                instance_id: catalog_instance_id(b),
                                node_ip: b.node_ip,
                                host_port: b.host_port,
                                healthy: b.healthy,
                                local: false,
                            })
                            .collect(),
                        firewall_allow_from: None,
                    };
                    merged.entries.insert(qualified.clone(), entry);
```

[src/bun/agent/consumer.rs:303–308](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L303-L308)

```rust
            .collect();
        let local = ServiceMap::from_snapshot(&local).map_err(failure)?;
        let merged =
            local.with_cluster_catalog_excluding_node(&catalog, Some(&owner.identity.node_id.0));
        let mut routes = self.ingress_configs.clone();
        let mut seen = std::collections::HashSet::new();
```

[src/bun/agent/consumer.rs:483–494](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L483-L494)

```rust
    /// service's entry in place.
    async fn install_consumer_view(
        &mut self,
        services: &[ServiceEntry],
        ingress: &[IngressAssignment],
    ) -> Result<(), BunError> {
        let map = ServiceMap::from_snapshot(services).map_err(failure)?;
        let routes = ingress
            .iter()
            .map(|route| {
                (
                    (route.namespace.clone(), route.name.clone()),
```
