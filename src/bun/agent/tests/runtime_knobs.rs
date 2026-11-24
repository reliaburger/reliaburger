//! Gated runtime knobs on a node (#1122): the node gate refuses a deploy
//! whose knobs `[runtime]` doesn't list, the runtime receives the knobs it
//! must apply, and the capability report advertises the allowlist only when
//! the runtime can apply it.

use super::commands::ApplyEvent;
use super::*;

fn knobbed_app() -> Config {
    Config::parse(
        "[app.lb]\nimage = \"lb:v1\"\n[app.lb.runtime]\ncapabilities = [\"IPC_OWNER\"]\n\
         sysctls = { \"net.core.somaxconn\" = \"4096\" }\n",
    )
    .unwrap()
}

fn allowing_both() -> crate::grill::knobs::RuntimeAllowlist {
    crate::config::NodeConfig::parse(
        "[runtime]\nallowed_capabilities = [\"IPC_OWNER\"]\nallowed_sysctls = [\"net.core.*\"]\n",
    )
    .unwrap()
    .runtime
    .knob_allowlist()
}

#[tokio::test]
async fn a_deploy_the_node_gate_refuses_never_reaches_the_runtime() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_applies_runtime_knobs(true);

    let events = drain_deploy(&mut agent, knobbed_app()).await;
    let Some(ApplyEvent::Error { message, .. }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    assert!(
        message.contains("runtime.capabilities CAP_IPC_OWNER refused by the node gate"),
        "{message}"
    );
    assert!(
        !grill.calls().iter().any(|(call, _)| call == "create"),
        "{:?}",
        grill.calls()
    );
}

#[tokio::test]
async fn a_deploy_both_gates_allow_hands_the_knobs_to_the_runtime() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_applies_runtime_knobs(true);
    agent.set_runtime_allowlist(allowing_both());

    expect_complete(&drain_deploy(&mut agent, knobbed_app()).await);

    let id = agent.supervisor.list_instances()[0].id.clone();
    let created = grill.created_spec(&id).expect("the runtime created it");
    assert_eq!(
        created.runtime_knobs,
        knobbed_app().app["lb"].runtime,
        "the runtime gets the knobs to apply"
    );
}

#[tokio::test]
async fn a_node_advertises_its_knobs_only_when_its_runtime_applies_them() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_runtime_allowlist(allowing_both());
    let (capabilities, _) = agent.live_egress_report_state().await;
    assert!(capabilities.runtime_knobs.is_empty());

    grill.set_applies_runtime_knobs(true);
    let (capabilities, _) = agent.live_egress_report_state().await;
    assert_eq!(capabilities.runtime_knobs, allowing_both());
}

/// How many times the runtime was asked to create `id`.
fn creates(grill: &MockGrill, id: &InstanceId) -> usize {
    grill
        .calls()
        .iter()
        .filter(|(call, instance)| call == "create" && instance == id)
        .count()
}

/// Crash `id`, then drive its restart to completion.
async fn crash_and_restart(agent: &mut TestAgent, grill: &MockGrill, id: &InstanceId) {
    grill.set_state(id, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
}

/// #1316: a restart builds the container from the spec stored at deploy,
/// so it used to skip the node gate. A node whose `[runtime]` allowlist
/// no longer lists a knob must refuse the restart, with the reason, and
/// not spend the restart budget retrying it.
#[tokio::test]
async fn a_restart_after_the_node_drops_a_knob_is_refused_with_the_reason() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_applies_runtime_knobs(true);
    agent.set_runtime_allowlist(allowing_both());
    expect_complete(&drain_deploy(&mut agent, knobbed_app()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();

    // The node restarts with a narrower `node.toml`.
    agent.set_runtime_allowlist(Default::default());
    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    let reason = instance.launch_failure.clone().unwrap_or_default();
    assert!(
        reason.contains("runtime.capabilities CAP_IPC_OWNER refused by the node gate"),
        "{reason}"
    );
    assert!(
        reason.contains("runtime.sysctls net.core.somaxconn"),
        "names every refused knob: {reason}"
    );
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_eq!(
        creates(&grill, &id),
        1,
        "only the deploy reached the runtime"
    );
}

/// The re-check refuses only what the node stopped allowing: a restart
/// the allowlist still covers creates the instance again.
#[tokio::test]
async fn a_restart_the_node_still_allows_creates_the_instance_again() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_applies_runtime_knobs(true);
    agent.set_runtime_allowlist(allowing_both());
    expect_complete(&drain_deploy(&mut agent, knobbed_app()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();

    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_ne!(instance.state, ContainerState::Failed);
    assert_eq!(instance.launch_failure, None);
    assert_eq!(creates(&grill, &id), 2);
}

/// #1316: host paths have the same double gate, and restarts skipped it
/// the same way. A source `[storage] allowed_host_paths` no longer covers
/// refuses the restart.
#[tokio::test]
async fn a_restart_after_the_node_drops_a_host_path_is_refused_with_the_reason() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(crate::bun::host_paths::HostPathPolicy::new(
        vec![source.path().to_path_buf()],
        Vec::new(),
    ));
    let config = Config::parse(&format!(
        "[app.web]\nimage = 'web:v1'\n[[app.web.volumes]]\npath = '/data'\nsource = '{}'\n",
        source.path().display()
    ))
    .unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let id = agent.supervisor.list_instances()[0].id.clone();

    agent.set_host_path_policy(Default::default());
    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    let reason = instance.launch_failure.clone().unwrap_or_default();
    assert!(reason.contains("host path refused"), "{reason}");
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_eq!(
        creates(&grill, &id),
        1,
        "only the deploy reached the runtime"
    );
}
