# Run v0.2.0-2026-10-18

Product: v0.2.0 (`cc5f64393350`, stable). Harness: `fb4615d202fb`. Generated from `results/v0.2.0-2026-10-18/`.

## Totals

| PASS | FAIL | UNSUPPORTED | BLOCKED | NOT_RUN | total |
|---|---|---|---|---|---|
| 57 | 18 | 4 | 7 | 0 | 86 |

Executed (PASS+FAIL): 75; pass rate of executed: 0.76. UNSUPPORTED and BLOCKED are never counted as passes.

## Vision

| id | status | scenario | reason |
|---|---|---|---|
| VIS-001 | PASS | Bootstrap and serve a multi-service application | all steps passed with recorded measurements |
| VIS-002[audit] | FAIL | The built-in audit log works with nothing else installed -- @7.1 audit | Then it should give the documented result within the configured time: AssertionError: 0... |
| VIS-002[batch] | PASS | The built-in task array works with nothing else installed -- @5.1 batch | all steps passed with recorded measurements |
| VIS-002[dashboard] | FAIL | The built-in dashboard works with nothing else installed -- @6.1 dashboard | Then it should give the documented result within the configured time: AssertionError: G... |
| VIS-002[logs] | PASS | The built-in logs works with nothing else installed -- @4.1 logs | all steps passed with recorded measurements |
| VIS-002[metrics] | PASS | The built-in metrics works with nothing else installed -- @3.1 metrics | all steps passed with recorded measurements |
| VIS-002[registry] | PASS | The built-in registry works with nothing else installed -- @2.1 registry | all steps passed with recorded measurements |
| VIS-002[secrets] | PASS | The built-in secrets works with nothing else installed -- @1.1 secrets | all steps passed with recorded measurements |
| VIS-002[top] | PASS | The built-in resource top works with nothing else installed -- @3.2 metrics | all steps passed with recorded measurements |

### VIS-001: Bootstrap and serve a multi-service application

Status PASS, 174 s, confidence high.

Claims: claim_two_binaries_everything_inside, claim_integrated_platform.

| measure | expected | measured |
|---|---|---|
| app_ready_s | <= 300 | 27.3 |
| formation_after_last_start_s | <= 180 | n/a |
| ingress /healthz on every node | 200 | n/a |
| frontend -> http://backend:9898/echo by name | 10/10 echoed | n/a |

### VIS-002[audit]: The built-in audit log works with nothing else installed -- @7.1 audit

Status FAIL, 4 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_audit_log_durable_bounded.

| measure | expected | measured |
|---|---|---|
| documented_result | true | false |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[batch]: The built-in task array works with nothing else installed -- @5.1 batch

Status PASS, 16 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_batch_jobs_one_lifecycle.

| measure | expected | measured |
|---|---|---|
| documented_result | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[dashboard]: The built-in dashboard works with nothing else installed -- @6.1 dashboard

Status FAIL, 5 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_brioche_dashboard_and_tui.

| measure | expected | measured |
|---|---|---|
| documented_result | true | false |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[logs]: The built-in logs works with nothing else installed -- @4.1 logs

Status PASS, 16 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_ketchup_logs_builtin.

| measure | expected | measured |
|---|---|---|
| documented_result | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[metrics]: The built-in metrics works with nothing else installed -- @3.1 metrics

Status PASS, 1 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_mayo_metrics_builtin.

| measure | expected | measured |
|---|---|---|
| documented_result | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[registry]: The built-in registry works with nothing else installed -- @2.1 registry

Status PASS, 1 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_pickle_registry_every_node.

| measure | expected | measured |
|---|---|---|
| podinfo_copies | >= 2 | 3 |
| documented_result | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

### VIS-002[secrets]: The built-in secrets works with nothing else installed -- @1.1 secrets

Status PASS, 180 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_secrets_age_encrypted_in_config.

| measure | expected | measured |
|---|---|---|
| documented_result | true | true |

- no reusable cluster: building fresh VMs and following docs/linux-servers.md

### VIS-002[top]: The built-in resource top works with nothing else installed -- @3.2 metrics

Status PASS, 1 s, confidence high, integration class `native`.

Claims: claim_integrated_platform, claim_mayo_metrics_builtin.

| measure | expected | measured |
|---|---|---|
| documented_result | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

## Extensibility

| id | status | scenario | reason |
|---|---|---|---|
| EXT-001 | PASS | Export application telemetry to an external collector | all steps passed with recorded measurements |
| EXT-002[audit] | PASS | relish audit --since 1h honours --output json -- @1.5 commands the claim names | all steps passed with recorded measurements |
| EXT-002[council] | PASS | relish council honours --output json -- @1.3 commands the claim names | all steps passed with recorded measurements |
| EXT-002[faults] | FAIL | relish fault list honours --output json -- @2.4 other read commands under the global --output flag | And its standard output should parse as JSON: AssertionError: stdout is not JSON (Expec... |
| EXT-002[history] | PASS | relish history frontend honours --output json -- @2.5 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-002[images] | PASS | relish images honours --output json -- @2.7 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-002[inspect] | FAIL | relish inspect frontend honours --output json -- @2.1 other read commands under the global --output flag | And its standard output should parse as JSON: AssertionError: stdout is not JSON (Expec... |
| EXT-002[jobs] | PASS | relish jobs honours --output json -- @2.6 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-002[metrics] | PASS | relish metrics frontend honours --output json -- @2.9 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-002[nodes] | PASS | relish nodes honours --output json -- @1.2 commands the claim names | all steps passed with recorded measurements |
| EXT-002[resolve] | FAIL | relish resolve backend honours --output json -- @2.3 other read commands under the global --output flag | And its standard output should parse as JSON: AssertionError: stdout is not JSON (Expec... |
| EXT-002[routes] | FAIL | relish routes honours --output json -- @2.2 other read commands under the global --output flag | And its standard output should parse as JSON: AssertionError: stdout is not JSON (Expec... |
| EXT-002[status] | PASS | relish status honours --output json -- @1.1 commands the claim names | all steps passed with recorded measurements |
| EXT-002[tokens] | PASS | relish token list honours --output json -- @1.4 commands the claim names | all steps passed with recorded measurements |
| EXT-002[top] | PASS | relish top honours --output json -- @2.8 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-002[wtf] | PASS | relish wtf honours --output json -- @2.10 other read commands under the global --output flag | all steps passed with recorded measurements |
| EXT-003 | PASS | A failed job run raises one signed webhook alert | all steps passed with recorded measurements |
| EXT-004[app-ingress] | PASS | Collect metrics through the application's own ingress at /metrics -- @1.3  | all steps passed with recorded measurements |
| EXT-004[node-api] | UNSUPPORTED | Collect metrics through the node API path /metrics -- @1.1  | no Prometheus surface: HTTP None; body starts 'URLError: <urlopen error [SSL: CERTIFICA... |
| EXT-004[remote-read] | UNSUPPORTED | Collect metrics through the node API path /v1/read -- @1.2  | no Prometheus surface: HTTP None; body starts 'URLError: <urlopen error [SSL: CERTIFICA... |
| EXT-005[admission] | FAIL | Extend the platform with an admission or validation webhook -- @1.3  | When I try to extend the platform with an admission or validation webhook: AssertionErr... |
| EXT-005[cli-plugin] | UNSUPPORTED | Extend the platform with a relish plugin executable on PATH -- @1.2  | relish does not dispatch relish-<name> executables on PATH: error: unrecognized subcomm... |
| EXT-005[custom-kind] | UNSUPPORTED | Extend the platform with a custom resource kind in the app config -- @1.1  | relish apply refuses a table it does not know: error: failed to parse TOML: TOML parse ... |
| EXT-006 | FAIL | An external controller reconciles a custom spec over the API | And the cluster event API should list events for the app: AssertionError: GET /v1/event... |
| EXT-007 | FAIL | Exported Parquet logs are readable by a standard tool | When I export the logs of the node running it to the workstation with relish logs-expor... |
| EXT-008 | FAIL | The cluster follows commits, reverts drift and honours deletes | Then the app should run with 2 replicas within the configured sync time: AssertionError... |

### EXT-001: Export application telemetry to an external collector

Status PASS, 40 s, confidence high, integration class `configured`, derived hypothesis.

Claims: claim_apps_export_traces_to_external_collectors, claim_no_otlp_opentelemetry_export, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| attributed_ratio | >= 0.9 within 90 s | 1.0 |
| product_source_modifications | recorded; none for a configured integration | [] |

- reused the healthy lab cluster built earlier in this or a previous run
- fixture image exports OTLP spans when run directly in Docker against the same collector
- best available path is 'configured'; platform-native OTLP export is unsupported

### EXT-002[audit]: relish audit --since 1h honours --output json -- @1.5 commands the claim names

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json audit --since 1h

### EXT-002[council]: relish council honours --output json -- @1.3 commands the claim names

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json council

### EXT-002[faults]: relish fault list honours --output json -- @2.4 other read commands under the global --output flag

Status FAIL, 5 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | false |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json fault list

### EXT-002[history]: relish history frontend honours --output json -- @2.5 other read commands under the global --output flag

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json history frontend

### EXT-002[images]: relish images honours --output json -- @2.7 other read commands under the global --output flag

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json images

### EXT-002[inspect]: relish inspect frontend honours --output json -- @2.1 other read commands under the global --output flag

Status FAIL, 5 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | false |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json inspect frontend

### EXT-002[jobs]: relish jobs honours --output json -- @2.6 other read commands under the global --output flag

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json jobs

### EXT-002[metrics]: relish metrics frontend honours --output json -- @2.9 other read commands under the global --output flag

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json metrics frontend

### EXT-002[nodes]: relish nodes honours --output json -- @1.2 commands the claim names

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json nodes

### EXT-002[resolve]: relish resolve backend honours --output json -- @2.3 other read commands under the global --output flag

Status FAIL, 4 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | false |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json resolve backend

### EXT-002[routes]: relish routes honours --output json -- @2.2 other read commands under the global --output flag

Status FAIL, 4 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | false |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json routes

### EXT-002[status]: relish status honours --output json -- @1.1 commands the claim names

Status PASS, 20 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json status

### EXT-002[tokens]: relish token list honours --output json -- @1.4 commands the claim names

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json token list

### EXT-002[top]: relish top honours --output json -- @2.8 other read commands under the global --output flag

Status PASS, 1 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json top

### EXT-002[wtf]: relish wtf honours --output json -- @2.10 other read commands under the global --output flag

Status PASS, 3 s, confidence high, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| exit_code | 0 | 0 |
| stdout_is_json | true | true |

- reused the healthy lab cluster built earlier in this or a previous run
- command: relish --output json wtf

### EXT-003: A failed job run raises one signed webhook alert

Status PASS, 192 s, confidence high, integration class `configured`, derived hypothesis.

Claims: claim_alert_webhooks_outbound, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| delivery_s | <= 90 | 10.5 |
| alert_fields | version 1, name job_run_failed, severity critical, status firing, timestamps set | {"version": "1", "alert.name": "job_run_failed", "alert.severity": "critical", "alert.s... |
| signature_valid | true | true |
| job_run_failed_alerts | 1 | 1 |

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-004[app-ingress]: Collect metrics through the application's own ingress at /metrics -- @1.3 

Status PASS, 1 s, confidence high, integration class `workaround`, derived hypothesis.

Claims: claim_app_metrics_scraping_and_extra_targets, claim_batteries_included, claim_no_prometheus_endpoint_remote_read_or_promql.

| measure | expected | measured |
|---|---|---|
| prometheus_exposition | true | true |

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-004[node-api]: Collect metrics through the node API path /metrics -- @1.1 

Status UNSUPPORTED, 34 s, confidence high, integration class `unsupported`, derived hypothesis.

Claims: claim_app_metrics_scraping_and_extra_targets, claim_batteries_included, claim_no_prometheus_endpoint_remote_read_or_promql.

| measure | expected | measured |
|---|---|---|
| prometheus_exposition | true | false |

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-004[remote-read]: Collect metrics through the node API path /v1/read -- @1.2 

Status UNSUPPORTED, 4 s, confidence high, integration class `unsupported`, derived hypothesis.

Claims: claim_app_metrics_scraping_and_extra_targets, claim_batteries_included, claim_no_prometheus_endpoint_remote_read_or_promql.

| measure | expected | measured |
|---|---|---|
| prometheus_exposition | true | false |

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-005[admission]: Extend the platform with an admission or validation webhook -- @1.3 

Status FAIL, 4 s, confidence medium, derived hypothesis.

Claims: claim_no_crds_operators_plugins_custom_schedulers, claim_batteries_included.

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-005[cli-plugin]: Extend the platform with a relish plugin executable on PATH -- @1.2 

Status UNSUPPORTED, 4 s, confidence high, derived hypothesis.

Claims: claim_no_crds_operators_plugins_custom_schedulers, claim_batteries_included.

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-005[custom-kind]: Extend the platform with a custom resource kind in the app config -- @1.1 

Status UNSUPPORTED, 4 s, confidence high, derived hypothesis.

Claims: claim_no_crds_operators_plugins_custom_schedulers, claim_batteries_included.

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-006: An external controller reconciles a custom spec over the API

Status FAIL, 35 s, confidence high, integration class `workaround`, derived hypothesis.

Claims: claim_cli_machine_readable_output, claim_batteries_included, claim_event_and_log_streaming_apis.

| measure | expected | measured |
|---|---|---|
| controller_converge_s | <= 120 | 12.7 |
| drift_revert_s | <= 60 | 17.6 |
| events_naming_app | >= 1 | 0 |

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-007: Exported Parquet logs are readable by a standard tool

Status FAIL, 9 s, confidence high, derived hypothesis.

Claims: claim_log_and_metric_export_to_object_storage, claim_batteries_included.

- reused the healthy lab cluster built earlier in this or a previous run

### EXT-008: The cluster follows commits, reverts drift and honours deletes

Status FAIL, 182 s, confidence high, integration class `configured`, derived hypothesis.

Claims: claim_lettuce_gitops_builtin, claim_batteries_included.

| measure | expected | measured |
|---|---|---|
| sync_s | <= 90 | null |

- reused the healthy lab cluster built earlier in this or a previous run

## Interoperability

| id | status | scenario | reason |
|---|---|---|---|
| INT-001 | PASS | Import a baseline Kubernetes application | all steps passed with recorded measurements |
| INT-002[ConfigMap] | PASS | Import a ConfigMap manifest -- @2.1 Documented as not wired into workloads | all steps passed with recorded measurements |
| INT-002[CronJob] | PASS | Import a CronJob manifest -- @1.8 Documented as converted | all steps passed with recorded measurements |
| INT-002[CustomResourceDefinition] | PASS | Import a CustomResourceDefinition manifest -- @3.3 Documented as dropped | all steps passed with recorded measurements |
| INT-002[CustomResource] | PASS | Import a CustomResource manifest -- @3.4 Documented as dropped | all steps passed with recorded measurements |
| INT-002[DaemonSet] | PASS | Import a DaemonSet manifest -- @1.3 Documented as converted | all steps passed with recorded measurements |
| INT-002[Deployment] | PASS | Import a Deployment manifest -- @1.1 Documented as converted | all steps passed with recorded measurements |
| INT-002[HorizontalPodAutoscaler] | PASS | Import a HorizontalPodAutoscaler manifest -- @1.6 Documented as converted | all steps passed with recorded measurements |
| INT-002[Ingress] | PASS | Import a Ingress manifest -- @1.5 Documented as converted | all steps passed with recorded measurements |
| INT-002[Job] | PASS | Import a Job manifest -- @1.7 Documented as converted | all steps passed with recorded measurements |
| INT-002[Namespace] | PASS | Import a Namespace manifest -- @1.9 Documented as converted | all steps passed with recorded measurements |
| INT-002[NetworkPolicy] | PASS | Import a NetworkPolicy manifest -- @3.1 Documented as dropped | all steps passed with recorded measurements |
| INT-002[PersistentVolumeClaim] | PASS | Import a PersistentVolumeClaim manifest -- @3.2 Documented as dropped | all steps passed with recorded measurements |
| INT-002[Role] | PASS | Import a Role manifest -- @3.6 Documented as dropped | all steps passed with recorded measurements |
| INT-002[Secret] | PASS | Import a Secret manifest -- @2.2 Documented as not wired into workloads | all steps passed with recorded measurements |
| INT-002[ServiceAccount] | PASS | Import a ServiceAccount manifest -- @3.5 Documented as dropped | all steps passed with recorded measurements |
| INT-002[Service] | PASS | Import a Service manifest -- @1.4 Documented as converted | all steps passed with recorded measurements |
| INT-002[StatefulSet] | PASS | Import a StatefulSet manifest -- @1.2 Documented as converted | all steps passed with recorded measurements |
| INT-003[online-boutique] | FAIL | Import Online Boutique v0.10.7 as published -- @1.2  | And every imported workload should be running within the configured time: AssertionErro... |
| INT-003[podinfo-webapp] | FAIL | Import podinfo 6.15.0 deploy/webapp as published -- @1.1  | When I apply them with relish apply -f in their own namespace: AssertionError: relish a... |
| INT-004 | PASS | A Reliaburger app exported to Kubernetes YAML runs on Kubernetes | all steps passed with recorded measurements |

### INT-001: Import a baseline Kubernetes application

Status PASS, 37 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_smooth_k8s_transition, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| silently_lost | [] | [] |
| probes | {"healthz": true, "configmap_message": true, "secret_env": true, "backend_by_name": true} | n/a |
| manual_steps_total | <= 6 | 4 |

- reused the healthy lab cluster built earlier in this or a previous run
- report says ConfigMap/fixture-config is 'not referenced by any workload' although the frontend references it through configMapKeyRef

### INT-002[ConfigMap]: Import a ConfigMap manifest -- @2.1 Documented as not wired into workloads

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | reported | n/a |

### INT-002[CronJob]: Import a CronJob manifest -- @1.8 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | schedule = "*/5 * * * *" | true |

### INT-002[CustomResourceDefinition]: Import a CustomResourceDefinition manifest -- @3.3 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[CustomResource]: Import a CustomResource manifest -- @3.4 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[DaemonSet]: Import a DaemonSet manifest -- @1.3 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | replicas = "*" | true |

### INT-002[Deployment]: Import a Deployment manifest -- @1.1 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | [app.web] | true |

### INT-002[HorizontalPodAutoscaler]: Import a HorizontalPodAutoscaler manifest -- @1.6 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | [app.web.autoscale] | true |

### INT-002[Ingress]: Import a Ingress manifest -- @1.5 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | host = "web.kinds.test" | true |

### INT-002[Job]: Import a Job manifest -- @1.7 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | [job.migrate] | true |

### INT-002[Namespace]: Import a Namespace manifest -- @1.9 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | [namespace.shop] | true |

### INT-002[NetworkPolicy]: Import a NetworkPolicy manifest -- @3.1 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[PersistentVolumeClaim]: Import a PersistentVolumeClaim manifest -- @3.2 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[Role]: Import a Role manifest -- @3.6 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[Secret]: Import a Secret manifest -- @2.2 Documented as not wired into workloads

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | reported | n/a |

### INT-002[ServiceAccount]: Import a ServiceAccount manifest -- @3.5 Documented as dropped

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | dropped | n/a |

### INT-002[Service]: Import a Service manifest -- @1.4 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | port = 9898 | true |

### INT-002[StatefulSet]: Import a StatefulSet manifest -- @1.2 Documented as converted

Status PASS, 0 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| outcome | converted | n/a |
| toml_contains | [app.db] | true |

### INT-003[online-boutique]: Import Online Boutique v0.10.7 as published -- @1.2 

Status FAIL, 621 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_smooth_k8s_transition, claim_k8s_apply_converts_eleven_kinds.

| measure | expected | measured |
|---|---|---|
| silently_dropped | [] | [] |
| workloads_running | 12 of 12 within 600 s | 11 of 12 |

- reused the healthy lab cluster built earlier in this or a previous run

### INT-003[podinfo-webapp]: Import podinfo 6.15.0 deploy/webapp as published -- @1.1 

Status FAIL, 4 s, confidence high.

Claims: claim_k8s_drops_reported_not_silent, claim_smooth_k8s_transition, claim_k8s_apply_converts_eleven_kinds.

- reused the healthy lab cluster built earlier in this or a previous run

### INT-004: A Reliaburger app exported to Kubernetes YAML runs on Kubernetes

Status PASS, 12 s, confidence high, integration class `native`.

Claims: claim_smooth_k8s_transition, claim_k8s_import_export_roundtrip.

| measure | expected | measured |
|---|---|---|
| export_report_names | health checks and metrics, which the fixture uses | n/a |

## Operations

| id | status | scenario | reason |
|---|---|---|---|
| OPS-001[leader] | FAIL | Recover replicas after a council-leader node is forcibly stopped -- @1.2  | And the application should satisfy the configured availability threshold during recover... |
| OPS-001[worker] | PASS | Recover replicas after a non-leader node is forcibly stopped -- @1.1  | all steps passed with recorded measurements |
| OPS-002[immediate] | FAIL | Delete and re-apply the fixture immediately after relish delete returns -- @1.2  | Then every cycle should reach the desired replicas within the configured convergence ti... |
| OPS-002[settled] | PASS | Delete and re-apply the fixture after relish status shows no instances -- @1.1  | all steps passed with recorded measurements |
| OPS-003[deployer-apply-other] | BLOCKED | A deployer token scoped to rbeval-rbac tries to apply an app in default -- @1.4  | setup: And a deployer API token scoped to rbeval-rbac: CommandFailed: command failed rc... |
| OPS-003[deployer-apply-own] | BLOCKED | A deployer token scoped to rbeval-rbac tries to apply an app in rbeval-rbac -- @1.3  | setup: And a deployer API token scoped to rbeval-rbac: CommandFailed: command failed rc... |
| OPS-003[deployer-namespace] | BLOCKED | A deployer token scoped to all tries to apply a namespace block -- @1.7  | setup: And a deployer API token scoped to all: CommandFailed: command failed rc=1: reli... |
| OPS-003[deployer-see-other] | BLOCKED | A deployer token scoped to rbeval-rbac tries to inspect an app in default -- @1.5  | setup: And a deployer API token scoped to rbeval-rbac: CommandFailed: command failed rc... |
| OPS-003[deployer-tokens] | BLOCKED | A deployer token scoped to all tries to create an API token -- @1.6  | setup: And a deployer API token scoped to all: CommandFailed: command failed rc=1: reli... |
| OPS-003[readonly-apply] | BLOCKED | A read-only token scoped to all tries to apply an app in rbeval-rbac -- @1.2  | setup: And a read-only API token scoped to all: CommandFailed: command failed rc=1: rel... |
| OPS-003[readonly-status] | BLOCKED | A read-only token scoped to all tries to read cluster status -- @1.1  | setup: And a read-only API token scoped to all: CommandFailed: command failed rc=1: rel... |
| OPS-004[operator-api] | PASS | Port the API port 9117 as seen from the operator -- @1.1  | all steps passed with recorded measurements |
| OPS-004[operator-gossip] | PASS | Port gossip port 9443 as seen from the operator -- @1.2  | all steps passed with recorded measurements |
| OPS-004[operator-raft] | PASS | Port Raft port 9444 as seen from the operator -- @1.3  | all steps passed with recorded measurements |
| OPS-004[operator-reporting] | PASS | Port reporting port 9445 as seen from the operator -- @1.4  | all steps passed with recorded measurements |
| OPS-004[outsider-api] | PASS | Port the API port 9117 as seen from a non-member -- @1.6  | all steps passed with recorded measurements |
| OPS-004[outsider-ingress] | PASS | Port ingress port 80 as seen from a non-member -- @1.8  | all steps passed with recorded measurements |
| OPS-004[outsider-raft] | PASS | Port Raft port 9444 as seen from a non-member -- @1.7  | all steps passed with recorded measurements |
| OPS-005[kill-all] | PASS | relish fault kill frontend --count 3 --acknowledge as admin -- @1.2  | all steps passed with recorded measurements |
| OPS-005[kill-one] | PASS | relish fault kill frontend --count 1 --acknowledge as admin -- @1.1  | all steps passed with recorded measurements |
| OPS-005[leader-node-kill] | PASS | relish fault node-kill LEADER --acknowledge as admin -- @1.6  | all steps passed with recorded measurements |
| OPS-005[no-ack] | PASS | relish fault kill frontend --count 1 as admin -- @1.3  | all steps passed with recorded measurements |
| OPS-005[pause-all] | PASS | relish fault pause frontend --acknowledge as admin -- @1.4  | all steps passed with recorded measurements |
| OPS-005[readonly-kill] | FAIL | relish fault kill frontend --count 1 --acknowledge as read-only -- @1.5  | When I request the fault "kill frontend --count 1 --acknowledge" with a read-only token... |
| OPS-006 | FAIL | Writes resume after the council leader loses power | Then a new leader should be elected within the configured election target: AssertionErr... |
| OPS-007 | PASS | One non-leader node is partitioned from its peers | all steps passed with recorded measurements |
| OPS-008 | PASS | A council voter loses power repeatedly while the cluster takes writes | all steps passed with recorded measurements |
| OPS-009 | FAIL | Every node loses power at once and is powered on again | And the applied apps should still be defined: AssertionError: running after power cycle... |
| OPS-010 | FAIL | A healthy change rolls out without downtime | And availability during the rollout should meet the configured threshold: AssertionErro... |
| OPS-011 | PASS | A change whose health check never passes is rolled back | all steps passed with recorded measurements |

### OPS-001[leader]: Recover replicas after a council-leader node is forcibly stopped -- @1.2 

Status FAIL, 99 s, confidence high.

Claims: claim_node_loss_reschedules_in_about_a_minute, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| detection_s | <= 30 | 0.6 |
| recovery_s | <= 120 | 39.6 |
| availability_ratio_via_surviving_ingresses | >= 0.95 | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-001[worker]: Recover replicas after a non-leader node is forcibly stopped -- @1.1 

Status PASS, 88 s, confidence high.

Claims: claim_node_loss_reschedules_in_about_a_minute, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| detection_s | <= 30 | 5.9 |
| recovery_s | <= 120 | 31.4 |
| availability_ratio_via_surviving_ingresses | >= 0.95 | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-002[immediate]: Delete and re-apply the fixture immediately after relish delete returns -- @1.2 

Status FAIL, 548 s, confidence high.

Claims: claim_self_healing, claim_apply_converges_after_delete.

| measure | expected | measured |
|---|---|---|
| cycles_converged | 5 of 5, each <= 300 s | 3 |

- reused the healthy lab cluster built earlier in this or a previous run
- cycle 4 did not converge within 300 s; later cycles were not attempted

### OPS-002[settled]: Delete and re-apply the fixture after relish status shows no instances -- @1.1 

Status PASS, 170 s, confidence high.

Claims: claim_self_healing, claim_apply_converges_after_delete.

| measure | expected | measured |
|---|---|---|
| cycles_converged | 5 of 5, each <= 300 s | 5 |
| non_running_fixture_instances | 0 | [] |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[deployer-apply-other]: A deployer token scoped to rbeval-rbac tries to apply an app in default -- @1.4 

Status BLOCKED, 4 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[deployer-apply-own]: A deployer token scoped to rbeval-rbac tries to apply an app in rbeval-rbac -- @1.3 

Status BLOCKED, 8 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[deployer-namespace]: A deployer token scoped to all tries to apply a namespace block -- @1.7 

Status BLOCKED, 4 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[deployer-see-other]: A deployer token scoped to rbeval-rbac tries to inspect an app in default -- @1.5 

Status BLOCKED, 4 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[deployer-tokens]: A deployer token scoped to all tries to create an API token -- @1.6 

Status BLOCKED, 4 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[readonly-apply]: A read-only token scoped to all tries to apply an app in rbeval-rbac -- @1.2 

Status BLOCKED, 4 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-003[readonly-status]: A read-only token scoped to all tries to read cluster status -- @1.1 

Status BLOCKED, 6 s, confidence high.

Claims: claim_integrated_platform, claim_token_roles_and_scoped_rbac.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[operator-api]: Port the API port 9117 as seen from the operator -- @1.1 

Status PASS, 1 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | open | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[operator-gossip]: Port gossip port 9443 as seen from the operator -- @1.2 

Status PASS, 10 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | filtered | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[operator-raft]: Port Raft port 9444 as seen from the operator -- @1.3 

Status PASS, 10 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | filtered | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[operator-reporting]: Port reporting port 9445 as seen from the operator -- @1.4 

Status PASS, 10 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | filtered | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[outsider-api]: Port the API port 9117 as seen from a non-member -- @1.6 

Status PASS, 10 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | filtered | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[outsider-ingress]: Port ingress port 80 as seen from a non-member -- @1.8 

Status PASS, 1 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | open | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-004[outsider-raft]: Port Raft port 9444 as seen from a non-member -- @1.7 

Status PASS, 10 s, confidence high.

Claims: claim_integrated_platform, claim_network_perimeter_firewall.

| measure | expected | measured |
|---|---|---|
| result_on_every_node | filtered | n/a |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[kill-all]: relish fault kill frontend --count 3 --acknowledge as admin -- @1.2 

Status PASS, 4 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

| measure | expected | measured |
|---|---|---|
| verdict | refuse | refuse |
| frontend_running_again_s | <= 120 | 0.1 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[kill-one]: relish fault kill frontend --count 1 --acknowledge as admin -- @1.1 

Status PASS, 21 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

| measure | expected | measured |
|---|---|---|
| verdict | accept | accept |
| frontend_running_again_s | <= 120 | 16.7 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[leader-node-kill]: relish fault node-kill LEADER --acknowledge as admin -- @1.6 

Status PASS, 5 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

| measure | expected | measured |
|---|---|---|
| verdict | refuse | refuse |
| frontend_running_again_s | <= 120 | 0.1 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[no-ack]: relish fault kill frontend --count 1 as admin -- @1.3 

Status PASS, 5 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

| measure | expected | measured |
|---|---|---|
| verdict | refuse | refuse |
| frontend_running_again_s | <= 120 | 0.1 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[pause-all]: relish fault pause frontend --acknowledge as admin -- @1.4 

Status PASS, 5 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

| measure | expected | measured |
|---|---|---|
| verdict | refuse | refuse |
| frontend_running_again_s | <= 120 | 0.1 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-005[readonly-kill]: relish fault kill frontend --count 1 --acknowledge as read-only -- @1.5 

Status FAIL, 6 s, confidence high.

Claims: claim_integrated_platform, claim_chaos_safety_policy.

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-006: Writes resume after the council leader loses power

Status FAIL, 55 s, confidence high.

Claims: claim_council_raft_quorum_and_self_healing, claim_leader_election_and_learning_period_targets, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| election_s | <= 5 | 11.6 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-007: One non-leader node is partitioned from its peers

Status PASS, 68 s, confidence high.

Claims: claim_partitions_no_split_brain_and_degradation, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| majority_write_placed_s | <= 20 | 9.8 |
| minority_apply | refused | {"rc": 1, "secs": 0.1, "stdout": "", "stderr": "error: API error (status 503): {\"error... |
| minority_ingress_ratio | >= 0.9 | 1.0 |
| rejoin_s | <= 120 | 2.5 |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-008: A council voter loses power repeatedly while the cluster takes writes

Status PASS, 117 s, confidence high.

Claims: claim_self_healing, claim_council_raft_quorum_and_self_healing.

| measure | expected | measured |
|---|---|---|
| cycles_rejoined | 3 of 3, each <= 180 s | 3 |
| corruption_lines | 0 | [] |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-009: Every node loses power at once and is powered on again

Status FAIL, 52 s, confidence high.

Claims: claim_council_raft_quorum_and_self_healing, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| council_healthy_s | <= 300 | 12.7 |
| fixture_serving_s | <= 300 | 32.7 |
| running_after | {"backend": 2, "frontend": 3} | {"backend": 1, "frontend": 1} |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-010: A healthy change rolls out without downtime

Status FAIL, 32 s, confidence high.

Claims: claim_self_healing, claim_rolling_deploys_health_gated_auto_rollback.

| measure | expected | measured |
|---|---|---|
| rollout_complete_s | <= 300 | 12.3 |
| availability_during_rollout | >= 0.99 | {"count": 39, "ok": 30, "ratio": 0.7692, "longest_failure_streak": 3, "p50_ms": 1.9, "p... |

- reused the healthy lab cluster built earlier in this or a previous run

### OPS-011: A change whose health check never passes is rolled back

Status PASS, 80 s, confidence high.

Claims: claim_rolling_deploys_health_gated_auto_rollback, claim_self_healing.

| measure | expected | measured |
|---|---|---|
| rollback_s | <= 180 | 71.5 |
| availability_during_rollout | >= 0.99 | {"count": 215, "ok": 215, "ratio": 1.0, "longest_failure_streak": 0, "p50_ms": 2.2, "p9... |

- reused the healthy lab cluster built earlier in this or a previous run

## Claim traceability

| claim | scenarios |
|---|---|
| claim_alert_webhooks_outbound | EXT-003 (PASS) |
| claim_app_metrics_scraping_and_extra_targets | EXT-004[node-api] (UNSUPPORTED), EXT-004[remote-read] (UNSUPPORTED), EXT-004[app-ingress] (PASS) |
| claim_apply_converges_after_delete | OPS-002[settled] (PASS), OPS-002[immediate] (FAIL) |
| claim_apps_export_traces_to_external_collectors | EXT-001 (PASS) |
| claim_audit_log_durable_bounded | VIS-002[audit] (FAIL) |
| claim_batch_jobs_one_lifecycle | VIS-002[batch] (PASS) |
| claim_batteries_included | EXT-003 (PASS), EXT-002[status] (PASS), EXT-002[nodes] (PASS), EXT-002[council] (PASS), EXT-002[tokens] (PASS), EXT-002[audit] (PASS), EXT-002[inspect] (FAIL), EXT-002[routes] (FAIL), EXT-002[resolve] (FAIL), EXT-002[faults] (FAIL), EXT-002[history] (PASS), EXT-002[jobs] (PASS), EXT-002[images] (PASS), EXT-002[top] (PASS), EXT-002[metrics] (PASS), EXT-002[wtf] (PASS), EXT-005[custom-kind] (UNSUPPORTED), EXT-005[cli-plugin] (UNSUPPORTED), EXT-005[admission] (FAIL), EXT-006 (FAIL), EXT-001 (PASS), EXT-008 (FAIL), EXT-007 (FAIL), EXT-004[node-api] (UNSUPPORTED), EXT-004[remote-read] (UNSUPPORTED), EXT-004[app-ingress] (PASS) |
| claim_brioche_dashboard_and_tui | VIS-002[dashboard] (FAIL) |
| claim_chaos_safety_policy | OPS-005[kill-one] (PASS), OPS-005[kill-all] (PASS), OPS-005[no-ack] (PASS), OPS-005[pause-all] (PASS), OPS-005[readonly-kill] (FAIL), OPS-005[leader-node-kill] (PASS) |
| claim_cli_machine_readable_output | EXT-002[status] (PASS), EXT-002[nodes] (PASS), EXT-002[council] (PASS), EXT-002[tokens] (PASS), EXT-002[audit] (PASS), EXT-002[inspect] (FAIL), EXT-002[routes] (FAIL), EXT-002[resolve] (FAIL), EXT-002[faults] (FAIL), EXT-002[history] (PASS), EXT-002[jobs] (PASS), EXT-002[images] (PASS), EXT-002[top] (PASS), EXT-002[metrics] (PASS), EXT-002[wtf] (PASS), EXT-006 (FAIL) |
| claim_council_raft_quorum_and_self_healing | OPS-009 (FAIL), OPS-006 (FAIL), OPS-008 (PASS) |
| claim_event_and_log_streaming_apis | EXT-006 (FAIL) |
| claim_integrated_platform | VIS-002[secrets] (PASS), VIS-002[registry] (PASS), VIS-002[metrics] (PASS), VIS-002[top] (PASS), VIS-002[logs] (PASS), VIS-002[batch] (PASS), VIS-002[dashboard] (FAIL), VIS-002[audit] (FAIL), VIS-001 (PASS), OPS-003[readonly-status] (BLOCKED), OPS-003[readonly-apply] (BLOCKED), OPS-003[deployer-apply-own] (BLOCKED), OPS-003[deployer-apply-other] (BLOCKED), OPS-003[deployer-see-other] (BLOCKED), OPS-003[deployer-tokens] (BLOCKED), OPS-003[deployer-namespace] (BLOCKED), OPS-005[kill-one] (PASS), OPS-005[kill-all] (PASS), OPS-005[no-ack] (PASS), OPS-005[pause-all] (PASS), OPS-005[readonly-kill] (FAIL), OPS-005[leader-node-kill] (PASS), OPS-004[operator-api] (PASS), OPS-004[operator-gossip] (PASS), OPS-004[operator-raft] (PASS), OPS-004[operator-reporting] (PASS), OPS-004[outsider-api] (PASS), OPS-004[outsider-raft] (PASS), OPS-004[outsider-ingress] (PASS) |
| claim_k8s_apply_converts_eleven_kinds | INT-002[Deployment] (PASS), INT-002[StatefulSet] (PASS), INT-002[DaemonSet] (PASS), INT-002[Service] (PASS), INT-002[Ingress] (PASS), INT-002[HorizontalPodAutoscaler] (PASS), INT-002[Job] (PASS), INT-002[CronJob] (PASS), INT-002[Namespace] (PASS), INT-002[ConfigMap] (PASS), INT-002[Secret] (PASS), INT-002[NetworkPolicy] (PASS), INT-002[PersistentVolumeClaim] (PASS), INT-002[CustomResourceDefinition] (PASS), INT-002[CustomResource] (PASS), INT-002[ServiceAccount] (PASS), INT-002[Role] (PASS), INT-001 (PASS), INT-003[podinfo-webapp] (FAIL), INT-003[online-boutique] (FAIL) |
| claim_k8s_drops_reported_not_silent | INT-002[Deployment] (PASS), INT-002[StatefulSet] (PASS), INT-002[DaemonSet] (PASS), INT-002[Service] (PASS), INT-002[Ingress] (PASS), INT-002[HorizontalPodAutoscaler] (PASS), INT-002[Job] (PASS), INT-002[CronJob] (PASS), INT-002[Namespace] (PASS), INT-002[ConfigMap] (PASS), INT-002[Secret] (PASS), INT-002[NetworkPolicy] (PASS), INT-002[PersistentVolumeClaim] (PASS), INT-002[CustomResourceDefinition] (PASS), INT-002[CustomResource] (PASS), INT-002[ServiceAccount] (PASS), INT-002[Role] (PASS), INT-001 (PASS), INT-003[podinfo-webapp] (FAIL), INT-003[online-boutique] (FAIL) |
| claim_k8s_import_export_roundtrip | INT-004 (PASS) |
| claim_ketchup_logs_builtin | VIS-002[logs] (PASS) |
| claim_leader_election_and_learning_period_targets | OPS-006 (FAIL) |
| claim_lettuce_gitops_builtin | EXT-008 (FAIL) |
| claim_log_and_metric_export_to_object_storage | EXT-007 (FAIL) |
| claim_mayo_metrics_builtin | VIS-002[metrics] (PASS), VIS-002[top] (PASS) |
| claim_network_perimeter_firewall | OPS-004[operator-api] (PASS), OPS-004[operator-gossip] (PASS), OPS-004[operator-raft] (PASS), OPS-004[operator-reporting] (PASS), OPS-004[outsider-api] (PASS), OPS-004[outsider-raft] (PASS), OPS-004[outsider-ingress] (PASS) |
| claim_no_crds_operators_plugins_custom_schedulers | EXT-005[custom-kind] (UNSUPPORTED), EXT-005[cli-plugin] (UNSUPPORTED), EXT-005[admission] (FAIL) |
| claim_no_otlp_opentelemetry_export | EXT-001 (PASS) |
| claim_no_prometheus_endpoint_remote_read_or_promql | EXT-004[node-api] (UNSUPPORTED), EXT-004[remote-read] (UNSUPPORTED), EXT-004[app-ingress] (PASS) |
| claim_node_loss_reschedules_in_about_a_minute | OPS-001[worker] (PASS), OPS-001[leader] (FAIL) |
| claim_partitions_no_split_brain_and_degradation | OPS-007 (PASS) |
| claim_pickle_registry_every_node | VIS-002[registry] (PASS) |
| claim_rolling_deploys_health_gated_auto_rollback | OPS-010 (FAIL), OPS-011 (PASS) |
| claim_secrets_age_encrypted_in_config | VIS-002[secrets] (PASS) |
| claim_self_healing | OPS-009 (FAIL), OPS-006 (FAIL), OPS-001[worker] (PASS), OPS-001[leader] (FAIL), OPS-007 (PASS), OPS-008 (PASS), OPS-002[settled] (PASS), OPS-002[immediate] (FAIL), OPS-010 (FAIL), OPS-011 (PASS) |
| claim_smooth_k8s_transition | INT-001 (PASS), INT-003[podinfo-webapp] (FAIL), INT-003[online-boutique] (FAIL), INT-004 (PASS) |
| claim_token_roles_and_scoped_rbac | OPS-003[readonly-status] (BLOCKED), OPS-003[readonly-apply] (BLOCKED), OPS-003[deployer-apply-own] (BLOCKED), OPS-003[deployer-apply-other] (BLOCKED), OPS-003[deployer-see-other] (BLOCKED), OPS-003[deployer-tokens] (BLOCKED), OPS-003[deployer-namespace] (BLOCKED) |
| claim_two_binaries_everything_inside | VIS-001 (PASS) |
