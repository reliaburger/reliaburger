//! Sidecars (#1120): containers that share an instance's network namespace
//! and lifecycle.
//!
//! Every test drives the agent against the mock runtime and reads its call
//! log, because the ordering is the feature: sidecars start after the init
//! containers and before their app, stop after it, restart alone when they
//! crash, and restart with it when it crashes.

use super::*;

const WEB: &str = "default__web-0";
const PROXY: &str = "default__web-0__sidecar-proxy";
const LOGS: &str = "default__web-0__sidecar-logs";

fn id(value: &str) -> InstanceId {
    InstanceId(value.to_string())
}

/// `web` with an init container and two sidecars, in that order.
fn web_with_sidecars(image: &str) -> Config {
    Config::parse(&format!(
        r#"
        [app.web]
        image = "{image}"
        port = 8080

        [[app.web.init]]
        command = ["migrate"]

        [[app.web.sidecar]]
        name = "proxy"
        image = "envoy:1.31"

        [[app.web.sidecar]]
        name = "logs"
        command = ["/bin/ship-logs"]
        "#
    ))
    .unwrap()
}

/// A mock runtime whose init container for `parent` exits 0 at once.
fn with_finished_init(grill: &MockGrill, parent: &str) {
    let init = id(&format!("{parent}__init-0"));
    grill.set_state(&init, ContainerState::Stopped);
    grill.set_exit_code(&init, Some(0));
}

/// Where `operation` on `instance` first appears in `calls`.
fn position(calls: &[(String, InstanceId)], operation: &str, instance: &InstanceId) -> usize {
    calls
        .iter()
        .position(|(op, id)| op == operation && id == instance)
        .unwrap_or_else(|| panic!("no {operation} of {instance} in {calls:?}"))
}

/// The operations on `instance` in `calls`, in order.
fn operations(calls: &[(String, InstanceId)], instance: &InstanceId) -> Vec<String> {
    calls
        .iter()
        .filter(|(_, id)| id == instance)
        .map(|(op, _)| op.clone())
        .collect()
}

#[tokio::test]
async fn sidecars_start_after_the_init_containers_and_before_their_app() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);

    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);

    let calls = grill.calls();
    let (web, init, proxy, logs) = (id(WEB), id(&format!("{WEB}__init-0")), id(PROXY), id(LOGS));
    // The app is created first: its network namespace is what they join.
    assert!(position(&calls, "create", &web) < position(&calls, "create", &proxy));
    assert!(position(&calls, "start", &init) < position(&calls, "create", &proxy));
    assert!(position(&calls, "create", &proxy) < position(&calls, "start", &proxy));
    // In the order the app declares them.
    assert!(position(&calls, "start", &proxy) < position(&calls, "create", &logs));
    assert!(position(&calls, "start", &logs) < position(&calls, "start", &web));

    let joined = grill.created_spec(&proxy).unwrap();
    assert_eq!(
        joined.joined_network(),
        Some("/var/run/netns/rb-default__web-0")
    );
    assert_eq!(joined.root.path, "envoy:1.31");
    assert_eq!(
        grill.created_spec(&logs).unwrap().root.path,
        "myapp:v1",
        "a sidecar without an image runs the app's"
    );
    assert_eq!(agent.sidecars.of(&web), vec![proxy, logs]);
}

#[tokio::test]
async fn an_app_stop_stops_its_sidecars_after_the_app_has_exited() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let from = grill.calls().len();

    agent.stop_app("web", "default").await.unwrap();

    let calls = grill.calls()[from..].to_vec();
    let (web, proxy, logs) = (id(WEB), id(PROXY), id(LOGS));
    let app_stopped = position(&calls, "stop", &web);
    // The app's exit is observed before any sidecar is signalled.
    let app_exit_seen = calls[app_stopped..]
        .iter()
        .position(|(op, instance)| op == "state" && instance == &web)
        .map(|offset| app_stopped + offset)
        .expect("the app's exit was never read");
    assert!(
        app_exit_seen < position(&calls, "stop", &proxy),
        "{calls:?}"
    );
    assert!(position(&calls, "stop", &proxy) < position(&calls, "stop", &logs));
    // Retirement kills whatever is left and forgets them.
    assert!(operations(&calls, &proxy).contains(&"kill".to_string()));
    assert!(agent.sidecars.of(&web).is_empty());
}

#[tokio::test]
async fn a_rolling_deploy_starts_new_sidecars_first_and_stops_old_ones_last() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let old = id(WEB);
    let old_proxy = id(PROXY);
    let from = grill.calls().len();

    let next = web_with_sidecars("myapp:v2");
    let new_id = |calls: &[(String, InstanceId)]| {
        calls
            .iter()
            .find(|(op, id)| {
                op == "create"
                    && id.0.starts_with("default__web-g")
                    && id.0.matches("__").count() == 1
            })
            .map(|(_, id)| id.clone())
    };
    // The replacement's init container exits at once too.
    for generation in 1..4 {
        with_finished_init(&grill, &format!("default__web-g{generation}-0"));
    }
    expect_complete(&drain_deploy(&mut agent, next).await);

    let calls = grill.calls()[from..].to_vec();
    let new = new_id(&calls).expect("no replacement was created");
    let new_proxy = crate::grill::oci::sidecar_instance_id(&new, "proxy");
    assert!(position(&calls, "start", &new_proxy) < position(&calls, "start", &new));
    assert_eq!(
        grill.created_spec(&new_proxy).unwrap().joined_network(),
        Some(format!("/var/run/netns/rb-{}", new.0).as_str()),
        "the new sidecar joins the new instance, not the old one"
    );
    assert!(position(&calls, "stop", &old) < position(&calls, "stop", &old_proxy));
    assert!(agent.sidecars.of(&old).is_empty());
    assert_eq!(agent.sidecars.of(&new).len(), 2);
}

#[tokio::test]
async fn a_sidecar_that_crashes_restarts_alone_and_its_app_keeps_running() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let (web, proxy, logs) = (id(WEB), id(PROXY), id(LOGS));
    let from = grill.calls().len();

    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;

    let calls = grill.calls()[from..].to_vec();
    let restarted: Vec<String> = operations(&calls, &proxy)
        .into_iter()
        .filter(|op| op != "state")
        .collect();
    assert_eq!(restarted, ["kill", "create", "start"], "{calls:?}");
    for op in ["stop", "kill", "create", "start"] {
        assert!(
            !operations(&calls, &web).contains(&op.to_string()),
            "the app was touched ({op}): {calls:?}"
        );
        assert!(
            !operations(&calls, &logs).contains(&op.to_string()),
            "the other sidecar was touched ({op}): {calls:?}"
        );
    }
    assert_eq!(
        agent.supervisor.get_instance(&web).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(agent.sidecars.restarts(&proxy), 1);
}

#[tokio::test]
async fn a_sidecar_that_keeps_crashing_waits_out_its_backoff() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let proxy = id(PROXY);

    // The first restart is at once.
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(agent.sidecars.restarts(&proxy), 1);

    // It's down again (the mock still reports it stopped): the second waits.
    let from = grill.calls().len();
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;
    assert!(
        !operations(&grill.calls()[from..], &proxy).contains(&"create".to_string()),
        "a second crash restarted at once"
    );
    assert_eq!(agent.sidecars.restarts(&proxy), 1);
}

#[tokio::test]
async fn an_app_that_crashes_restarts_with_its_sidecars_stopped_after_and_started_before_it() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let (web, proxy, logs) = (id(WEB), id(PROXY), id(LOGS));
    let from = grill.calls().len();

    grill.set_state(&web, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;

    let calls = grill.calls()[from..].to_vec();
    // Clear: the app first, then its sidecars, which lose its namespace.
    assert!(position(&calls, "kill", &web) < position(&calls, "kill", &proxy));
    assert!(position(&calls, "kill", &proxy) < position(&calls, "kill", &logs));
    // Create, then the sidecars, then the app.
    assert!(position(&calls, "create", &web) < position(&calls, "create", &proxy));
    assert!(position(&calls, "start", &proxy) < position(&calls, "start", &web));
    assert!(position(&calls, "start", &logs) < position(&calls, "start", &web));
    assert_eq!(
        agent.supervisor.get_instance(&web).unwrap().state,
        ContainerState::Running
    );
}

#[tokio::test]
async fn a_sidecar_that_cannot_start_fails_its_instance_which_never_starts() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    grill.fail_create_of(&id(LOGS));

    let events = drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the failed sidecar produced no Error event");
    assert!(message.contains("sidecar-logs"), "{message}");
    let calls = grill.calls();
    let web = id(WEB);
    assert!(
        !operations(&calls, &web).contains(&"start".to_string()),
        "the app started without its sidecar: {calls:?}"
    );
    // The sidecar that did start goes with its failed instance.
    let proxy = id(PROXY);
    assert!(position(&calls, "start", &proxy) < position(&calls, "kill", &proxy));
}

#[tokio::test]
async fn a_node_whose_runtime_cannot_share_a_network_refuses_sidecars_with_a_reason() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_shares_network(false);

    let events = drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("sidecars on a runtime that can't share a network were accepted");
    assert!(message.contains("network namespace"), "{message}");
    assert!(
        !grill.calls().iter().any(|(op, _)| op == "create"),
        "nothing should reach the runtime"
    );
}

#[tokio::test]
async fn the_capability_report_says_whether_the_runtime_can_run_sidecars() {
    let (agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let (capabilities, _) = agent.live_egress_report_state().await;
    assert!(capabilities.shared_network);

    grill.set_shares_network(false);
    let (capabilities, _) = agent.live_egress_report_state().await;
    assert!(!capabilities.shared_network);
}

/// Sidecars have no adoption records, so a Bun restart's startup retires
/// them with every other unrecorded launch. The adopted instance gets them
/// back on the next tick, joined to its namespace.
#[tokio::test]
async fn an_adopted_instance_gets_its_sidecars_started_again() {
    let records = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    let mut record = adoption_record(WEB, "web", false);
    record.app_spec.as_mut().unwrap().sidecar =
        web_with_sidecars("myapp:v1").app["web"].sidecar.clone();
    crate::grill::records::write_record(records.path(), &record).unwrap();
    grill.set_adopt_result(&id(WEB), true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    assert_eq!(agent.sidecars.of(&id(WEB)), [id(PROXY), id(LOGS)]);
    let from = grill.calls().len();
    agent.relaunch_due_sidecars_to_completion().await;

    let calls = grill.calls()[from..].to_vec();
    for sidecar in [id(PROXY), id(LOGS)] {
        let restarted: Vec<String> = operations(&calls, &sidecar)
            .into_iter()
            .filter(|op| op != "state")
            .collect();
        assert_eq!(
            restarted,
            ["kill", "create", "start"],
            "{sidecar}: {calls:?}"
        );
        assert_eq!(
            grill.created_spec(&sidecar).unwrap().joined_network(),
            Some("/var/run/netns/rb-default__web-0")
        );
    }
    assert!(
        operations(&calls, &id(WEB)).iter().all(|op| op == "state"),
        "the adopted instance was disturbed: {calls:?}"
    );
}
