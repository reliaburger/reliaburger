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

/// #1434: a rollout stores the new spec before its old instances go. A
/// sidecar of an old instance that crashes meanwhile comes back as its
/// instance launched it: its old image, not a sidecar the new spec added,
/// and still there when the new spec dropped it.
#[tokio::test]
async fn a_sidecar_that_crashes_mid_rollout_comes_back_from_its_own_instances_spec() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let (proxy, logs) = (id(PROXY), id(LOGS));
    let next = Config::parse(
        r#"
        [app.web]
        image = "myapp:v2"
        port = 8080

        [[app.web.sidecar]]
        name = "proxy"
        image = "envoy:1.32"

        [[app.web.sidecar]]
        name = "metrics"
        image = "exporter:1"
        "#,
    )
    .unwrap();
    // What the deploy worker's store_deployed_spec does as the rollout begins.
    agent.deployed_specs.insert(
        ("web".to_string(), "default".to_string()),
        next.app["web"].clone(),
    );
    let from = grill.calls().len();

    grill.set_state(&proxy, ContainerState::Stopped);
    grill.set_state(&logs, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;

    let calls = grill.calls()[from..].to_vec();
    assert_eq!(
        grill.created_spec(&proxy).unwrap().root.path,
        "envoy:1.31",
        "the old instance's proxy came back from the new spec"
    );
    assert!(
        operations(&calls, &logs).contains(&"create".to_string()),
        "a sidecar the new spec dropped never came back: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|(_, instance)| instance.0.ends_with("__sidecar-metrics")),
        "a sidecar only the new spec has was started beside an old instance: {calls:?}"
    );
    assert!(
        agent
            .sidecars
            .due(std::time::Instant::now() + std::time::Duration::from_secs(120))
            .is_empty()
    );
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
            ApplyEvent::Error { message, .. } => Some(message.clone()),
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
            ApplyEvent::Error { message, .. } => Some(message.clone()),
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

/// Sidecars with no adoption records of their own (lost, or never written)
/// are retired at startup with every other unrecorded launch. The adopted
/// instance gets them back on the next tick, joined to its namespace.
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

/// An instance that never passes its startup probe is stopped with cleanup
/// owed. A sidecar restart still in flight then must give up: the cleanup
/// kills the sidecars it knows, and a relaunch that started its sidecar
/// after that kill left it running beside a Failed instance (#1999).
#[tokio::test]
async fn a_sidecar_restarting_when_its_instance_is_given_up_does_not_survive() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let config = Config::parse(
        r#"
        [app.web]
        image = "myapp:v1"
        port = 8080

        [app.web.health]
        path = "/healthz"

        [[app.web.sidecar]]
        name = "proxy"
        image = "envoy:1.31"
        "#,
    )
    .unwrap();
    let (deploy_events, _receiver) = mpsc::channel(64);
    agent.deploy(config, &deploy_events).await;
    let (web, proxy) = (id(WEB), id(PROXY));
    assert_eq!(
        agent.supervisor.get_instance(&web).unwrap().state,
        ContainerState::HealthWait
    );
    let from = grill.calls().len();

    // The proxy crashes, and its restart is under way: past its kill, slow
    // to create.
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    grill.set_call_delay(
        crate::grill::mock::MockCall::Create,
        Some(std::time::Duration::from_secs(1)),
    );
    agent.relaunch_due_sidecars();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(agent.sidecars.relaunching(&web));

    // Meanwhile the startup grace runs out, and the cleanup stops the
    // instance and its sidecars.
    let created_at = agent.supervisor.get_instance(&web).unwrap().created_at;
    while agent.supervisor.get_instance(&web).unwrap().state == ContainerState::HealthWait {
        agent
            .complete_health_probe(
                web.clone(),
                created_at,
                Ok(crate::bun::health::HealthStatus::ConnectionRefused),
            )
            .await;
    }
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&web).unwrap().state,
        ContainerState::Failed
    );

    // The relaunch finishes after the cleanup.
    agent.relaunch_due_sidecars_to_completion().await;
    let proxy_operations: Vec<String> = operations(&grill.calls()[from..], &proxy)
        .into_iter()
        .filter(|op| op != "state")
        .collect();
    let last_start = proxy_operations.iter().rposition(|op| op == "start");
    let last_kill = proxy_operations.iter().rposition(|op| op == "kill");
    assert!(
        last_start < last_kill,
        "the proxy was left running beside its failed instance: {proxy_operations:?}"
    );
}

// --- Adoption across a Bun restart (#1315) ---

/// The sidecar records under `records`, by instance id, once `count` of
/// them have landed: a launch writes them from a task.
async fn sidecar_records(
    records: &std::path::Path,
    count: usize,
) -> Vec<crate::grill::records::InstanceRecord> {
    let directory = crate::grill::records::sidecar_records_dir(records);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut found = crate::grill::records::load_records(&directory).unwrap();
            if found.len() >= count {
                found.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fewer than {count} sidecar records landed"))
}

/// Deploy `web` with its two sidecars on an agent that keeps records in
/// `records`, and wait for the sidecars' records.
async fn deployed_with_records(
    records: &std::path::Path,
) -> (
    TestAgent,
    MockGrill,
    Vec<crate::grill::records::InstanceRecord>,
) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.to_path_buf());
    grill.set_pid(std::process::id());
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let recorded = sidecar_records(records, 2).await;
    (agent, grill, recorded)
}

/// A restarted Bun on the same node: a fresh agent and runtime handle over
/// the records the last one left.
fn restarted_agent(records: &std::path::Path) -> (TestAgent, MockGrill) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.to_path_buf());
    grill.set_pid(std::process::id());
    (agent, grill)
}

/// Each started sidecar gets an adoption record of its own, beside its
/// instance's, naming its process and the spec it was launched from.
#[tokio::test]
async fn a_started_sidecar_is_recorded_for_adoption() {
    let records = tempfile::tempdir().unwrap();
    let (agent, grill, recorded) = deployed_with_records(records.path()).await;

    let ids: Vec<&str> = recorded.iter().map(|r| r.instance_id.as_str()).collect();
    assert_eq!(ids, [LOGS, PROXY]);
    for record in &recorded {
        assert_eq!(record.app_name, "web");
        assert_eq!(record.namespace, "default");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.host_port, None, "a sidecar holds no port");
        let sidecar = id(&record.instance_id);
        assert!(
            grill
                .created_spec(&sidecar)
                .unwrap()
                .matches_journal(&record.oci_spec),
            "{sidecar}: the record names another spec than its launch"
        );
    }
    assert_eq!(recorded[1].image, "envoy:1.31");
    assert_eq!(recorded[0].image, "myapp:v1", "logs runs the app's image");
    // The instance's own record is still the only one in the main directory.
    let main = crate::grill::records::load_records(records.path()).unwrap();
    assert_eq!(main.len(), 1);
    assert_eq!(main[0].instance_id, WEB);
    drop(agent);
}

/// The gap #1315 closed: a Bun restart adopts the running sidecars with
/// their instance, so nothing restarts and the sidecars keep their
/// process. Before, startup retired them and the next tick started them
/// again a second later.
#[tokio::test]
async fn a_bun_restart_adopts_running_sidecars_instead_of_restarting_them() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, _) = deployed_with_records(records.path()).await;
    drop(first);

    let (mut agent, grill) = restarted_agent(records.path());
    for instance in [WEB, PROXY, LOGS] {
        grill.set_adopt_result(&id(instance), true);
    }
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let (web, proxy, logs) = (id(WEB), id(PROXY), id(LOGS));
    assert_eq!(agent.sidecars.of(&web), [proxy.clone(), logs.clone()]);
    assert_eq!(
        agent.sidecars.running_beside(&web),
        [proxy.clone(), logs.clone()],
        "adopted sidecars are running, not due"
    );
    agent.relaunch_due_sidecars_to_completion().await;
    let calls = grill.calls();
    for sidecar in [&proxy, &logs] {
        let touched: Vec<String> = operations(&calls, sidecar)
            .into_iter()
            .filter(|op| op != "state")
            .collect();
        assert_eq!(touched, ["adopt"], "{sidecar}: {calls:?}");
        assert_eq!(agent.sidecars.restarts(sidecar), 0);
    }
    // Their records stay, for the next restart.
    assert_eq!(sidecar_records(records.path(), 2).await.len(), 2);
}

/// A sidecar whose record the runtime won't vouch for (its process is gone,
/// or another runs under its name) is retired and starts again beside its
/// adopted instance, as every sidecar did before #1315.
#[tokio::test]
async fn a_sidecar_the_runtime_will_not_adopt_starts_again() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, _) = deployed_with_records(records.path()).await;
    drop(first);

    let (mut agent, grill) = restarted_agent(records.path());
    grill.set_adopt_result(&id(WEB), true);
    grill.set_adopt_result(&id(PROXY), true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let (web, proxy, logs) = (id(WEB), id(PROXY), id(LOGS));
    assert_eq!(
        agent.sidecars.running_beside(&web),
        std::slice::from_ref(&proxy)
    );
    let from = grill.calls().len();
    agent.relaunch_due_sidecars_to_completion().await;
    let calls = grill.calls()[from..].to_vec();
    let restarted: Vec<String> = operations(&calls, &logs)
        .into_iter()
        .filter(|op| op != "state")
        .collect();
    assert_eq!(restarted, ["kill", "create", "start"], "{calls:?}");
    assert!(
        operations(&calls, &proxy).iter().all(|op| op == "state"),
        "the adopted sidecar was disturbed: {calls:?}"
    );
    // The restarted one is recorded again.
    let recorded = sidecar_records(records.path(), 2).await;
    assert!(recorded.iter().any(|record| record.instance_id == LOGS));
}

/// An instance that isn't adopted takes its recorded sidecars with it:
/// they mustn't run beside nothing.
#[tokio::test]
async fn the_sidecars_of_an_instance_that_is_not_adopted_are_retired() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, _) = deployed_with_records(records.path()).await;
    drop(first);

    let (mut agent, grill) = restarted_agent(records.path());
    grill.set_adopt_result(&id(PROXY), true);
    grill.set_adopt_result(&id(LOGS), true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);

    let calls = grill.calls();
    for sidecar in [id(PROXY), id(LOGS)] {
        let touched = operations(&calls, &sidecar);
        assert!(!touched.contains(&"adopt".to_string()), "{calls:?}");
        assert!(touched.contains(&"kill".to_string()), "{calls:?}");
    }
    assert!(agent.sidecars.of(&id(WEB)).is_empty());
    let directory = crate::grill::records::sidecar_records_dir(records.path());
    assert!(
        crate::grill::records::load_records(&directory)
            .unwrap()
            .is_empty(),
        "the retired sidecars' records stayed"
    );
}

/// A sidecar restarted on its own records its new process, and an instance
/// stopped for good removes its sidecars' records.
#[tokio::test]
async fn a_restarted_sidecar_is_recorded_again_and_a_stop_removes_the_records() {
    let records = tempfile::tempdir().unwrap();
    let (mut agent, grill, _) = deployed_with_records(records.path()).await;
    let directory = crate::grill::records::sidecar_records_dir(records.path());
    crate::grill::records::remove_record(&directory, PROXY).unwrap();

    grill.set_state(&id(PROXY), ContainerState::Stopped);
    agent.check_apps().await;
    grill.set_state(&id(PROXY), ContainerState::Running);
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(agent.sidecars.restarts(&id(PROXY)), 1);
    sidecar_records(records.path(), 2).await;

    agent.stop_app("web", "default").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !crate::grill::records::load_records(&directory)
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stopped instance's sidecar records stayed");
}

/// #1315: the node's status carries each instance's sidecars, so `relish
/// apps` can show them. Before, a sidecar showed up only as Restart events.
#[tokio::test]
async fn an_instances_status_lists_its_sidecars_with_their_restarts() {
    use crate::bun::agent::SidecarState;
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    with_finished_init(&grill, WEB);
    expect_complete(&drain_deploy(&mut agent, web_with_sidecars("myapp:v1")).await);
    let sidecars_of_web = |agent: &TestAgent| {
        agent
            .status_entries()
            .into_iter()
            .find(|entry| entry.status.id == WEB)
            .unwrap()
            .status
            .sidecars
            .into_iter()
            .map(|sidecar| (sidecar.id, sidecar.state, sidecar.restart_count))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        sidecars_of_web(&agent),
        [
            (PROXY.to_string(), SidecarState::Running, 0),
            (LOGS.to_string(), SidecarState::Running, 0)
        ]
    );

    grill.set_state(&id(PROXY), ContainerState::Stopped);
    agent.check_apps().await;
    assert_eq!(sidecars_of_web(&agent)[0].1, SidecarState::Backoff);
    grill.set_state(&id(PROXY), ContainerState::Running);
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(
        sidecars_of_web(&agent)[0],
        (PROXY.to_string(), SidecarState::Running, 1)
    );
}

/// On a runtime that lists its launches, as runc does, startup keeps a
/// recorded sidecar's launch for adoption and still retires a sidecar
/// launch beside no recorded instance.
#[tokio::test]
async fn startup_keeps_recorded_sidecar_launches_and_retires_the_rest() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, sidecars) = deployed_with_records(records.path()).await;
    drop(first);
    let main = crate::grill::records::load_records(records.path()).unwrap();
    let launch = |id: &str, spec: &crate::grill::oci::OciSpec| crate::grill::RuntimeLaunch {
        instance_id: InstanceId(id.to_string()),
        spec: spec.clone(),
        generation: crate::grill::RuntimeGeneration::process(id),
        network_reference: None,
    };
    const ORPHAN: &str = "default__gone-0__sidecar-proxy";
    let mut launches = vec![launch(WEB, &main[0].oci_spec)];
    launches.extend(
        sidecars
            .iter()
            .map(|record| launch(&record.instance_id, &record.oci_spec)),
    );
    launches.push(launch(ORPHAN, &sidecars[0].oci_spec));

    let (mut agent, grill) = restarted_agent(records.path());
    grill.set_launch_inventory(launches).await;
    for instance in [WEB, PROXY, LOGS, ORPHAN] {
        grill.set_adopt_result(&id(instance), true);
    }
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let calls = grill.calls();
    for sidecar in [id(PROXY), id(LOGS)] {
        let touched = operations(&calls, &sidecar);
        assert!(!touched.contains(&"kill".to_string()), "{calls:?}");
        assert!(touched.contains(&"adopt".to_string()), "{calls:?}");
    }
    let orphan = operations(&calls, &id(ORPHAN));
    assert!(orphan.contains(&"kill".to_string()), "{calls:?}");
    assert!(!orphan.contains(&"adopt".to_string()), "{calls:?}");
    assert_eq!(
        agent.sidecars.running_beside(&id(WEB)),
        [id(PROXY), id(LOGS)]
    );
}

/// Review of #2269: a sidecar that exited while Bun was down is refused by
/// the runtime's adoption, and its retirement makes runc forget it. Startup
/// then killed it a second time, runc answered `NotFound`, and Bun didn't
/// start. The mock forgets for real here, as runc does.
#[tokio::test]
async fn a_sidecar_that_died_while_bun_was_down_is_retired_once_and_bun_starts() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, _) = deployed_with_records(records.path()).await;
    drop(first);

    let (mut agent, grill) = restarted_agent(records.path());
    grill.forget_retired_for_real();
    grill.set_adopt_result(&id(WEB), true);
    grill.set_adopt_result(&id(PROXY), true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let calls = grill.calls();
    let logs = operations(&calls, &id(LOGS));
    let forgets = logs.iter().filter(|op| *op == "forget_retired").count();
    assert_eq!(forgets, 1, "{calls:?}");
    let forgot = logs.iter().position(|op| op == "forget_retired").unwrap();
    assert!(
        !logs[forgot..].iter().any(|op| op == "kill"),
        "a forgotten sidecar was killed again: {calls:?}"
    );
    assert_eq!(agent.sidecars.running_beside(&id(WEB)), [id(PROXY)]);
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(
        agent.sidecars.running_beside(&id(WEB)),
        [id(PROXY), id(LOGS)]
    );
}

/// Review of #2269: a sidecar the runtime can't vouch for either way is
/// best effort, as its records are. It's retired and starts again; the node
/// still starts, with its instance and the other sidecar adopted.
#[tokio::test]
async fn a_sidecar_whose_adoption_fails_starts_again_and_bun_still_starts() {
    let records = tempfile::tempdir().unwrap();
    let (first, _grill, _) = deployed_with_records(records.path()).await;
    drop(first);

    let (mut agent, grill) = restarted_agent(records.path());
    grill.forget_retired_for_real();
    for instance in [WEB, PROXY, LOGS] {
        grill.set_adopt_result(&id(instance), true);
    }
    grill.set_adopt_failure(&id(LOGS));
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let logs = operations(&grill.calls(), &id(LOGS));
    assert!(logs.contains(&"kill".to_string()), "{logs:?}");
    assert_eq!(agent.sidecars.running_beside(&id(WEB)), [id(PROXY)]);
    let from = grill.calls().len();
    agent.relaunch_due_sidecars_to_completion().await;
    let restarted: Vec<String> = operations(&grill.calls()[from..], &id(LOGS))
        .into_iter()
        .filter(|op| op != "state")
        .collect();
    assert_eq!(restarted.last().map(String::as_str), Some("start"));
}
