//! Gated runtime knobs on a node (#1122): the node gate refuses a deploy
//! whose knobs `[runtime]` doesn't list, the runtime receives the knobs it
//! must apply, and the capability report advertises the allowlist only when
//! the runtime can apply it.

use super::commands::ApplyEvent;
use super::*;

fn knobbed_app() -> Config {
    Config::parse(
        "[app.lb]\nimage = \"lb:v1\"\n[app.lb.runtime]\ncapabilities = [\"NET_ADMIN\"]\n\
         sysctls = { \"net.core.somaxconn\" = \"4096\" }\n",
    )
    .unwrap()
}

fn allowing_both() -> crate::grill::knobs::RuntimeAllowlist {
    crate::config::NodeConfig::parse(
        "[runtime]\nallowed_capabilities = [\"NET_ADMIN\"]\nallowed_sysctls = [\"net.core.*\"]\n",
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
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    assert!(
        message.contains("runtime.capabilities CAP_NET_ADMIN refused by the node gate"),
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
