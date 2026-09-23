//! The portable integration suite, compiled as one test binary.
//!
//! Every file directly under `tests/` is its own crate, and each one links
//! the whole `reliaburger` library. Dozens of small binaries meant dozens of
//! links and gigabytes of debug executables, so the small, ungated files live
//! here as modules of a single binary instead. A test's nextest name is
//! `<module>::<test>`.
//!
//! Gated, heavy or process-sensitive suites stay as separate binaries under
//! `tests/`; see `docs/design/test-harness.md`.

#[path = "../support/bun_process.rs"]
mod bun_process;
#[path = "../support/cluster.rs"]
mod cluster_support;
// `cluster.rs` already includes the task harness; loading the file a second
// time would compile two distinct `TestTasks` types.
use cluster_support::task_harness;

mod agent_cluster;
mod api_tls;
mod api_tokens;
mod batch;
mod bun_auth_startup;
mod compatibility;
mod council_persistence;
mod council_tcp;
mod declarative_resources;
mod dependency_audit;
mod dev_robustness;
mod dns;
mod endpoint_withdrawal;
mod gitops;
mod ingress;
mod ingress_certificates;
mod ingress_connection_lifetime;
mod ingress_file_reload;
mod ingress_sessions;
mod live_node_identity;
mod log_export;
mod logs_cross_node;
mod managed_status;
mod metrics_aggregation;
mod node_issuer_validity;
mod node_renewal;
mod onion;
mod owned_commands;
mod parquet_safety;
mod pickle_cluster;
mod pickle_integrity;
mod process_owner;
mod reconstruction;
mod registry_authority;
mod registry_capability;
mod registry_routable_push;
mod registry_upload;
mod relish_cli;
mod reporting_tree;
mod runtime_executor;
mod security_integration;
mod service_endpoints;
mod tls_connection_lifetime;
mod uninstall;
mod website;
mod workload_trust_domain;
mod wtf_watch;
