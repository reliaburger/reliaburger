# apply --dry-run calls materially changed workloads unchanged when the image is unchanged

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Plan generated with changed replicas, port and env reproduced.

### Problem and impact


The live dry-run preview compares only each app/job’s image string. It reports unchanged for updates to replicas, resources, environment, port, health, command/script or deployment settings when the image stays the same. Namespace and permission resources are also considered unchanged merely because their names exist. This hides operational and authorization changes from an operator using the preview to review an apply.

The input/output contract cannot compute a full diff: `CurrentResource` carries only a bare `resource` key and `image`. The `/v1/apps` cluster aggregation additionally collapses same-named apps from different namespaces under `app.<name>`.

### Reproduction and verified result


Generate a production plan with current resource `{resource: "app.web", image: "web:v1"}` and this desired config:

```toml
[app.web]
image = "web:v1"
replicas = 9
port = 9999
[app.web.env]
MODE = "changed"
```

The executable test observes `PlanAction::Unchanged` and `to_update = 0`. The same-image comparator is identical for jobs. The bare-name status merge is source-verified.

### Fix direction and acceptance


Fetch namespace-qualified desired specs or stable canonical fingerprints and compare every field that apply may change. Use desired-state evidence rather than treating running image identity as the whole configuration. If complete comparison is unavailable, explicitly label it unknown/partial rather than unchanged. Retain the documented offline preview behavior while making unavailable comparison visible.

- Preview detects replica/resource/env/port/health/command/deployment changes without an image change.
- Preview detects permission grants and namespace quota changes.
- Same-named apps in different namespaces do not overwrite each other’s evidence.
- Unchanged is reserved for equivalent effective desired specs, and rendered totals agree with the actions.

### Existing issue comparison


No matching issue was found in the inventory. This concerns the existing `apply --dry-run` preview, not a deferred separate plan command.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/plan.rs:130–141](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/plan.rs#L130-L141)

```rust
                    PlanAction::Update
                } else {
                    PlanAction::Unchanged
                }
            }
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary,
        });
```

[src/relish/client.rs:1093–1104](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/client.rs#L1093-L1104)

```rust
        Ok(rows
            .into_iter()
            .map(|row| crate::relish::plan::CurrentResource {
                resource: row.resource,
                image: row.image,
            })
            .collect())
    }

    /// Trigger an immediate log export on the agent (`POST /v1/logs/export`).
    ///
    /// The destination is resolved agent-side — a path on the agent host,
```

[src/bun/api/status.rs:29–41](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/status.rs#L29-L41)

```rust
    }

    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for (app_id, spec) in &desired.apps {
            resources.insert(format!("app.{}", app_id.name), spec.image.clone());
        }
        for name in desired.namespaces.keys() {
            resources.insert(format!("namespace.{name}"), None);
        }
        for name in desired.permissions.keys() {
            resources.insert(format!("permission.{name}"), None);
        }
```
