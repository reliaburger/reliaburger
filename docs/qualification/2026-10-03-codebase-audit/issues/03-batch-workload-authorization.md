# Batch submission bypasses token workload scope and Deploy/HostExec grants

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Authenticated router dispatch reproduced.

### Problem


`POST /v1/batch` checks only the caller's Deployer role and then dispatches every supplied job directly to the agent. A token scoped to app `allowed` in namespace `allowedns` can submit a job named `forbidden` in `forbiddenns`. The normal `/v1/apply` route confines every app/job to token scope and checks `Deploy`, plus `HostExec` for scripts/binaries. Batch does none of these checks.

An unscoped Deployer denied `HostExec` by namespace policy can also use batch to run an allowlisted host script. The node's binary allowlist still applies; this defect bypasses the per-principal authorization layer, not the allowlist. Follower submission forwards using the cluster service token, so merely adding a leader-side check without preserving the original authorization would still grant the user system authority.

### Evidence


- [src/bun/batch.rs:700–711](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L711): role check and early forwarding, before job inspection.
- [src/bun/batch.rs:731–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L731-L778): namespace resolution followed by allocation; no scoped/grant admission.
- [src/bun/batch.rs:499–508](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L499-L508): `AgentCommand::Deploy` bypasses the HTTP apply admission checks.
- [src/bun/batch.rs:1068–1075](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L1068-L1075): user request is forwarded with the service token.
- [src/bun/api/apply.rs:267–307](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L267-L307): corresponding scope, Deploy and HostExec enforcement in the ordinary route.

Executed router probe `evidence/batch.rs` with a genuine Argon2-authenticated Deployer token confined to `allowed`/`allowedns`. Submission of the forbidden job returned `202 Accepted`, `assigned:1`, and the fake command consumer observed `AgentCommand::Deploy` containing `forbidden`. This verifies HTTP authentication, handler acceptance and dispatch, without executing host commands.

### Expected behavior / fix direction


Apply the normal scope, namespace permission, host-execution and test-lease/image admission checks to every job **before** registering or dispatching any batch. Preserve the user's credential when forwarding submission, just as `cluster_apply` does. Test submission through both leader and follower, mixed allowed/disallowed jobs, and denied HostExec; refusals must create no tracker record or agent command.

Existing F05 (#363) concerns identity lifecycle and audiences, not this already-supported scoped credential bypass. Closed #298 concerns read-route authorization, not batch job execution.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:700–712](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L712)

```rust
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    // Followers forward the raw body to the leader (the tracker and
    // the aggregated capacity view live there).
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body).await;
    }
```

[src/bun/batch.rs:731–745](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L731-L745)

```rust
    // One namespace per job, resolved here and used everywhere (JOB3).
    let mut jobs = match resolve_job_namespaces(request.jobs) {
        Ok(jobs) => jobs,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    };
    // Stable input order: together with the scheduler's ordered
    // profile groups this pins the assignment plan (the old
    // allocation-order finding).
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
```

[src/bun/batch.rs:491–508](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L491-L508)

```rust
        Err(e) => {
            eprintln!("bun: batch config synthesis failed: {e}");
            for job in &jobs {
                reporter.report(batch_id, &job.name, false).await;
            }
            return;
        }
    };
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    if cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
```

[src/bun/batch.rs:1063–1078](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L1063-L1078)

```rust
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no cluster leader known yet; retry shortly" })),
        )
            .into_response();
    };
    let mut request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}{path}"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    proxy_response(request.send().await).await
}
```

[src/bun/api/apply.rs:273–299](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L273-L299)

```rust
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }
```
