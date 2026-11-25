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
    assert_restart_refusal(&launch_failure(&agent, &id), source.path());
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_eq!(
        creates(&grill, &id),
        1,
        "only the deploy reached the runtime"
    );
}

// --- #2335: the host-path re-check reads the restarting instance's own
// spec, covers sidecars, and is bounded. ---

/// A policy that admits host paths under `directories` only.
fn allowing(directories: &[&std::path::Path]) -> crate::bun::host_paths::HostPathPolicy {
    crate::bun::host_paths::HostPathPolicy::new(
        directories.iter().map(|path| path.to_path_buf()).collect(),
        Vec::new(),
    )
}

/// `web`, image `image`, mounting `source` at `/data`, with `extra` TOML
/// appended to the app.
fn web_mounting(image: &str, source: &std::path::Path, extra: &str) -> Config {
    Config::parse(&format!(
        "[app.web]\nimage = '{image}'\n[[app.web.volumes]]\npath = '/data'\nsource = '{}'\n{extra}",
        source.display()
    ))
    .unwrap()
}

/// The instance's launch failure, which `relish apps` and `relish wtf` show.
fn launch_failure(agent: &TestAgent, id: &InstanceId) -> String {
    agent
        .supervisor
        .get_instance(id)
        .unwrap()
        .launch_failure
        .clone()
        .unwrap_or_default()
}

/// The refusal says where it happened and why, once: at a restart, a path
/// the deploy approved isn't allowed any more, naming the path.
fn assert_restart_refusal(reason: &str, source: &std::path::Path) {
    let source = std::fs::canonicalize(source).unwrap();
    assert!(
        reason.contains(&source.display().to_string()),
        "names the path: {reason}"
    );
    assert!(
        reason.contains("restart"),
        "says it was a restart: {reason}"
    );
    assert!(
        reason.contains("isn't allowed on this node any more"),
        "says the node no longer allows it: {reason}"
    );
    assert!(
        !reason.contains("host path refused: host path"),
        "says it once: {reason}"
    );
}

/// #2335: a rollout stores the new spec before its old instances go. An
/// old instance that crashes meanwhile restarts from its own stored spec,
/// so the gate must judge its own mounts, not the new spec's: here the
/// node now allows only the path the new spec mounts.
#[tokio::test]
async fn an_old_instance_restarting_mid_rollout_is_checked_against_its_own_host_paths() {
    let (old_source, new_source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[old_source.path(), new_source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_mounting("web:v1", old_source.path(), "")).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    // What the deploy worker's store_deployed_spec does as the rollout begins.
    agent.deployed_specs.insert(
        ("web".to_string(), "default".to_string()),
        web_mounting("web:v2", new_source.path(), "").app["web"].clone(),
    );

    agent.set_host_path_policy(allowing(&[new_source.path()]));
    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_restart_refusal(&launch_failure(&agent, &id), old_source.path());
    assert_eq!(
        creates(&grill, &id),
        1,
        "only the deploy reached the runtime"
    );
}

/// #2335: a restart re-checks what its stored spec mounts even when the
/// agent holds no deployed spec for the app, as after a Bun restart that
/// adopted the instance from its record and hasn't seen a deploy since.
#[tokio::test]
async fn an_adopted_instance_that_crashes_under_a_narrower_gate_is_refused() {
    let source = tempfile::tempdir().unwrap();
    let records = tempfile::tempdir().unwrap();
    let id = InstanceId("default__web-0".into());
    {
        let (mut first, _tx, _shutdown, grill) = test_agent_with_grill();
        first.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        first.set_host_path_policy(allowing(&[source.path()]));
        expect_complete(&drain_deploy(&mut first, web_mounting("web:v1", source.path(), "")).await);
        let record = crate::grill::records::record_path(records.path(), &id.0);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !record.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the record never landed");
    }

    // Bun restarts with a `node.toml` that no longer lists the source.
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    grill.set_pid(std::process::id());
    grill.set_adopt_result(&id, true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running,
        "a running container is adopted, whatever the gate says now"
    );

    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_restart_refusal(&launch_failure(&agent, &id), source.path());
    assert_eq!(
        creates(&grill, &id),
        0,
        "the restart never reached the runtime"
    );
}

/// #2335: resolving a host path can hang on a source whose NFS or autofs
/// server doesn't answer. The check is bounded by the runtime's
/// confirmation timeout, as the restart's kill is, and running out of time
/// is a failed attempt that's retried, not a refusal: the path may answer
/// next time.
#[tokio::test]
async fn a_host_path_check_that_runs_out_of_time_fails_the_restart_and_retries_it() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_mounting("web:v1", source.path(), "")).await);
    let id = agent.supervisor.list_instances()[0].id.clone();

    agent.set_stop_confirmation_timeout(std::time::Duration::from_millis(50));
    agent.set_host_path_policy(
        allowing(&[source.path()]).stalling(std::time::Duration::from_millis(500)),
    );
    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_ne!(
        instance.state,
        ContainerState::Failed,
        "a timeout isn't a refusal"
    );
    assert!(instance.retry_pending, "the restart is tried again");
    assert_eq!(instance.launch_failure, None);
    assert_eq!(
        creates(&grill, &id),
        1,
        "only the deploy reached the runtime"
    );

    // The source answers again, and the retry, once its backoff is over,
    // creates the instance.
    agent.set_host_path_policy(allowing(&[source.path()]));
    agent.drive_pending_restarts_to_completion().await;
    agent.supervisor.get_instance_mut(&id).unwrap().last_restart = None;
    agent.drive_pending_restarts_to_completion().await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(creates(&grill, &id), 2);
}

/// `web` mounting `source`, with a proxy sidecar.
fn web_with_proxy(source: &std::path::Path) -> Config {
    web_mounting(
        "web:v1",
        source,
        "[[app.web.sidecar]]\nname = 'proxy'\nimage = 'envoy:1.31'\n",
    )
}

const WEB: &str = "default__web-0";
const PROXY: &str = "default__web-0__sidecar-proxy";

/// Whether the runtime was asked to stop, kill, create or start `id`
/// since call `from`.
fn touched(grill: &MockGrill, id: &InstanceId, from: usize) -> bool {
    grill.calls()[from..]
        .iter()
        .any(|(call, instance)| instance == id && call != "state")
}

/// #2335: a sidecar's host-path check that runs out of time is a failed
/// restart of the sidecar alone, tried again after its backoff. It's not a
/// refusal, so the app keeps running and nothing stops it.
#[tokio::test]
async fn a_sidecar_whose_host_path_check_runs_out_of_time_is_retried_and_its_app_runs_on() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_with_proxy(source.path())).await);
    let (web, proxy) = (InstanceId(WEB.into()), InstanceId(PROXY.into()));

    agent.set_stop_confirmation_timeout(std::time::Duration::from_millis(50));
    agent.set_host_path_policy(
        allowing(&[source.path()]).stalling(std::time::Duration::from_millis(300)),
    );
    let from = grill.calls().len();
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;

    assert!(
        !touched(&grill, &web, from),
        "the app was touched: {:?}",
        grill.calls()
    );
    let instance = agent.supervisor.get_instance(&web).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(instance.launch_failure, None);
    assert_eq!(creates(&grill, &proxy), 1, "the sidecar was created anyway");
    assert_eq!(
        agent
            .sidecars
            .due(std::time::Instant::now() + std::time::Duration::from_secs(120)),
        [(web.clone(), proxy.clone())],
        "the sidecar is due again after its backoff"
    );

    // The source answers again, and the retry after the backoff starts it.
    agent.set_host_path_policy(allowing(&[source.path()]));
    tokio::time::sleep(super::super::sidecars::sidecar_backoff(1)).await;
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(creates(&grill, &proxy), 2);
    assert!(
        !touched(&grill, &web, from),
        "the app was touched: {:?}",
        grill.calls()
    );
}

/// #2335: a check stuck on a source that doesn't answer never returns, so
/// each retry used to leave one more blocking thread behind it, about one a
/// minute without limit. A retry while the first check is still stuck
/// starts no new check, and fails as the first did.
#[tokio::test]
async fn a_sidecar_retry_while_its_first_host_path_check_is_stuck_starts_no_new_check() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_with_proxy(source.path())).await);
    let proxy = InstanceId(PROXY.into());

    agent.set_stop_confirmation_timeout(std::time::Duration::from_millis(20));
    let stuck = allowing(&[source.path()]).stalling(std::time::Duration::from_secs(2));
    agent.set_host_path_policy(stuck.clone());
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(stuck.checks_started(), 1);

    tokio::time::sleep(super::super::sidecars::sidecar_backoff(1)).await;
    agent.relaunch_due_sidecars_to_completion().await;

    assert_eq!(agent.sidecars.restarts(&proxy), 2, "the retry ran");
    assert_eq!(
        stuck.checks_started(),
        1,
        "the retry started a second check"
    );
    assert_eq!(creates(&grill, &proxy), 1);
}

/// The refusal recorded on `parent`'s sidecar `sidecar`, if it's parked.
fn sidecar_refusal(agent: &TestAgent, parent: &InstanceId, sidecar: &InstanceId) -> Option<String> {
    agent
        .sidecars
        .statuses(parent, true)
        .into_iter()
        .find(|status| status.id == sidecar.0)
        .and_then(|status| status.refused)
}

/// #2335: a sidecar mounts its app's volumes, and a sidecar that crashes
/// on its own is created again from them. After the node stops allowing
/// one, the sidecar's restart is refused before the runtime sees it. It's
/// parked with the reason, and its app keeps running: a node never kills a
/// running instance over a policy change (#1316). The app's own next
/// restart is refused by the same check.
#[tokio::test]
async fn a_sidecar_restart_after_the_node_drops_a_host_path_is_refused_and_its_app_runs_on() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_with_proxy(source.path())).await);
    let (web, proxy) = (InstanceId(WEB.into()), InstanceId(PROXY.into()));
    assert_eq!(creates(&grill, &proxy), 1);

    agent.set_host_path_policy(Default::default());
    let from = grill.calls().len();
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;
    agent.drive_pending_restarts_to_completion().await;

    assert_eq!(
        creates(&grill, &proxy),
        1,
        "the refused sidecar was created again"
    );
    assert!(
        agent
            .sidecars
            .due(std::time::Instant::now() + std::time::Duration::from_secs(120))
            .is_empty(),
        "the refused sidecar is parked, not due again"
    );
    assert!(
        !touched(&grill, &web, from),
        "the app was touched: {:?}",
        grill.calls()
    );
    let instance = agent.supervisor.get_instance(&web).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(instance.launch_failure, None);
    let status = agent
        .sidecars
        .statuses(&web, true)
        .into_iter()
        .find(|status| status.id == PROXY)
        .unwrap();
    assert_eq!(status.state, crate::bun::agent::SidecarState::Stopped);
    let reason = status.refused.unwrap_or_default();
    assert!(
        reason.contains("restart of sidecar"),
        "names the sidecar: {reason}"
    );
    assert_restart_refusal(&reason, source.path());

    // A later sweep leaves the parked sidecar alone.
    agent.check_apps().await;
    agent.relaunch_due_sidecars_to_completion().await;
    assert_eq!(creates(&grill, &proxy), 1);

    // The app's own next restart is refused, as #1316 has it.
    crash_and_restart(&mut agent, &grill, &web).await;
    let instance = agent.supervisor.get_instance(&web).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    assert!(!instance.retry_pending, "a refused restart is not retried");
    assert_restart_refusal(&launch_failure(&agent, &web), source.path());
    assert_eq!(
        creates(&grill, &web),
        1,
        "only the deploy reached the runtime"
    );
}

/// #2335: the instance cancels a sidecar's restart when it crashes itself,
/// because it restarts with its sidecars. A refusal that relaunch brings
/// back afterwards is about a launch that's gone, and must leave the
/// restarting instance to its own restart, which asks the gate itself.
#[tokio::test]
async fn a_cancelled_sidecar_restart_that_comes_back_refused_leaves_its_instance_alone() {
    let source = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_host_path_policy(allowing(&[source.path()]));
    expect_complete(&drain_deploy(&mut agent, web_with_proxy(source.path())).await);
    let (web, proxy) = (InstanceId(WEB.into()), InstanceId(PROXY.into()));

    // The gate narrows, and the sidecar's check is slow to refuse.
    agent.set_host_path_policy(
        crate::bun::host_paths::HostPathPolicy::default()
            .stalling(std::time::Duration::from_millis(200)),
    );
    grill.set_state(&proxy, ContainerState::Stopped);
    agent.check_apps().await;
    agent.relaunch_due_sidecars();
    // Meanwhile the app crashes too, which cancels the sidecar's restart.
    grill.set_state(&web, ContainerState::Stopped);
    agent.check_apps().await;
    let before = agent.supervisor.get_instance(&web).unwrap().state;
    agent.relaunch_due_sidecars_to_completion().await;

    let instance = agent.supervisor.get_instance(&web).unwrap();
    assert_eq!(
        instance.state, before,
        "the late refusal moved the instance"
    );
    assert_eq!(instance.launch_failure, None);
    assert_eq!(
        sidecar_refusal(&agent, &web, &proxy),
        None,
        "a refusal for a cancelled restart parked the sidecar"
    );
}

/// #2335: a record written before the spec named its host paths carries
/// none, as an instance adopted after an in-place upgrade from 0.3.2 does.
/// Its restart isn't re-checked: this pins the documented gap, which its
/// next deploy closes.
#[tokio::test]
async fn an_adopted_instance_whose_record_names_no_host_paths_restarts_unchecked() {
    let source = tempfile::tempdir().unwrap();
    let records = tempfile::tempdir().unwrap();
    let mut record = adoption_record(WEB, "web", false);
    record.app_spec.as_mut().unwrap().volumes = vec![crate::config::types::VolumeSpec {
        path: "/data".into(),
        source: Some(source.path().to_path_buf()),
        size: None,
    }];
    record.oci_spec.mounts.push(crate::grill::oci::OciMount {
        destination: "/data".into(),
        source: Some(source.path().to_path_buf()),
        mount_type: Some("bind".into()),
        options: vec!["bind".into(), "rw".into()],
    });
    assert!(record.oci_spec.host_path_sources.is_empty());
    crate::grill::records::write_record(records.path(), &record).unwrap();

    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    let id = InstanceId(WEB.into());
    grill.set_adopt_result(&id, true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    // The node allows no host path at all.
    crash_and_restart(&mut agent, &grill, &id).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_ne!(instance.state, ContainerState::Failed);
    assert_eq!(instance.launch_failure, None);
    assert_eq!(creates(&grill, &id), 1, "the restart created it");
}
