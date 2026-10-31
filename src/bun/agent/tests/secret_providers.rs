//! Exec secret providers at launch (#1123): a `PROVIDER[...]` env value is
//! resolved just before the runtime creates the instance, a provider that
//! fails fails that instance closed with its reason, and nothing else is
//! affected.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::commands::{AgentCommand, ApplyEvent};
use super::*;
use crate::sesame::secret_provider::{
    MAX_TIMEOUT_SECS, ProviderName, SecretProviderConfig, SecretProviders,
};

/// A value no status, event or error may ever carry.
const SECRET: &str = "s3cret-from-the-provider";

/// Write an executable `/bin/sh` provider script.
fn provider_script(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("provider");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A provider that answers with [`SECRET`].
fn answering(dir: &Path) -> PathBuf {
    provider_script(
        dir,
        &format!("head -n 1 > /dev/null\nprintf '{{\"version\":1,\"value\":\"{SECRET}\"}}'"),
    )
}

/// The timeout for a provider whose test isn't about time. A loaded host can
/// take seconds just to start `/bin/sh`, so these get the longest a node
/// accepts, and only the slow-provider test judges the timeout itself.
const PATIENT: u64 = MAX_TIMEOUT_SECS;

/// The node's `vault` provider, running `binary` with a `timeout_secs` timeout.
fn vault(binary: &Path, timeout_secs: u64) -> SecretProviders {
    let config = SecretProviderConfig {
        timeout_secs,
        ..SecretProviderConfig::new(binary)
    };
    SecretProviders::new(BTreeMap::from([(
        ProviderName::try_from("vault".to_string()).unwrap(),
        config,
    )]))
}

fn provider_app(name: &str) -> Config {
    Config::parse(&format!(
        "[app.{name}]\nimage = \"api:v1\"\n[app.{name}.env]\nDB_PASSWORD = \"PROVIDER[vault:secret/db#password]\"\nMODE = \"plain\"\n"
    ))
    .unwrap()
}

fn env_value(spec: &crate::grill::oci::OciSpec, name: &str) -> Option<String> {
    spec.process
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix(&format!("{name}=")).map(str::to_string))
}

/// Every instance's status, from the running loop.
async fn statuses(tx: &mpsc::Sender<AgentCommand>) -> Vec<super::super::status::InstanceStatus> {
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    result.await.unwrap()
}

#[tokio::test]
async fn a_provider_value_reaches_the_runtime_but_never_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&answering(dir.path()), PATIENT));

    expect_complete(&drain_deploy(&mut agent, provider_app("api")).await);

    let id = agent.supervisor.list_instances()[0].id.clone();
    let created = grill.created_spec(&id).expect("the runtime created it");
    assert_eq!(env_value(&created, "DB_PASSWORD").as_deref(), Some(SECRET));
    assert_eq!(env_value(&created, "MODE").as_deref(), Some("plain"));

    // What the node keeps for restarts holds the reference, not the secret.
    let kept = agent
        .supervisor
        .get_instance(&id)
        .and_then(|instance| instance.oci_spec.clone())
        .expect("the spec is kept for restarts");
    assert_eq!(
        env_value(&kept, "DB_PASSWORD").as_deref(),
        Some("PROVIDER[vault:secret/db#password]")
    );
}

/// #1498: an init container's own env reaches the runtime, a provider
/// reference in it resolved like the app's, on top of the app's env.
#[tokio::test]
async fn an_init_containers_own_env_reaches_the_runtime_with_its_provider_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&answering(dir.path()), PATIENT));
    let init = InstanceId("default__loadgen-0__init-0".to_string());
    grill.set_state(&init, ContainerState::Stopped);
    grill.set_exit_code(&init, Some(0));
    let handle = tokio::spawn(async move { agent.run().await });

    let config = Config::parse(
        r#"
        [app.loadgen]
        image = "loadgen:v1"
        env = { MODE = "app" }

        [[app.loadgen.init]]
        command = ["wait-for-frontend"]
        env = { FRONTEND_ADDR = "frontend:80", TOKEN = "PROVIDER[vault:loadgen/token]" }
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;
    shutdown.cancel();
    handle.await.unwrap();
    expect_complete(&events);

    let created = grill.created_spec(&init).expect("the init was created");
    assert_eq!(
        env_value(&created, "FRONTEND_ADDR").as_deref(),
        Some("frontend:80")
    );
    assert_eq!(env_value(&created, "TOKEN").as_deref(), Some(SECRET));
    assert_eq!(env_value(&created, "MODE").as_deref(), Some("app"));
}

/// #1498: an init container whose provider doesn't answer fails its
/// instance closed, as the app's own reference would, and is never created
/// with the reference as its value.
#[tokio::test]
async fn an_init_containers_slow_provider_fails_the_instance_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&provider_script(dir.path(), "sleep 30"), 1));
    let handle = tokio::spawn(async move { agent.run().await });

    let config = Config::parse(
        r#"
        [app.loadgen]
        image = "loadgen:v1"

        [[app.loadgen.init]]
        command = ["wait-for-frontend"]
        env = { TOKEN = "PROVIDER[vault:loadgen/token]" }
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    let reason = "cannot resolve secret TOKEN: secret provider \"vault\" did not answer within 1s and was killed";
    assert!(message.contains(reason), "{message}");

    let instances = statuses(&tx).await;
    shutdown.cancel();
    handle.await.unwrap();
    let loadgen = instances
        .iter()
        .find(|instance| instance.app_name == "loadgen")
        .expect("the failed instance stays listed");
    assert_eq!(loadgen.state, "failed");
    assert_eq!(loadgen.reason.as_deref(), Some(reason));
    // The app's own container is created before its inits run, and stopped
    // here without ever starting.
    let init = InstanceId("default__loadgen-0__init-0".to_string());
    assert!(
        grill.created_spec(&init).is_none(),
        "an init without its secret must never be created: {:?}",
        grill.calls()
    );
}

#[tokio::test]
async fn a_slow_provider_fails_the_instance_closed_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&provider_script(dir.path(), "sleep 30"), 1));
    let handle = tokio::spawn(async move { agent.run().await });

    let events = send_deploy(&tx, provider_app("api")).await;
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    let reason = "cannot resolve secret DB_PASSWORD: secret provider \"vault\" did not answer within 1s and was killed";
    assert!(message.contains(reason), "{message}");

    let instances = statuses(&tx).await;
    shutdown.cancel();
    handle.await.unwrap();
    let api = instances
        .iter()
        .find(|instance| instance.app_name == "api")
        .expect("the failed instance stays listed");
    assert_eq!(api.state, "failed");
    assert_eq!(api.reason.as_deref(), Some(reason));
    assert!(
        !grill.calls().iter().any(|(call, _)| call == "create"),
        "an instance without its secret must never be created: {:?}",
        grill.calls()
    );
}

#[tokio::test]
async fn a_provider_the_node_lacks_fails_closed_and_unrelated_apps_still_start() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let handle = tokio::spawn(async move { agent.run().await });

    let events = send_deploy(&tx, provider_app("api")).await;
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    assert!(
        message.contains("secret provider \"vault\" is not configured on this node"),
        "{message}"
    );

    // An app that names no provider deploys as if nothing happened.
    let plain = Config::parse("[app.web]\nimage = \"web:v1\"\n").unwrap();
    expect_complete(&send_deploy(&tx, plain).await);

    let instances = statuses(&tx).await;
    shutdown.cancel();
    handle.await.unwrap();
    let state = |app: &str| {
        instances
            .iter()
            .find(|instance| instance.app_name == app)
            .map(|instance| instance.state.clone())
    };
    assert_eq!(state("api").as_deref(), Some("failed"));
    assert_ne!(state("web").as_deref(), Some("failed"));
    assert!(
        grill.calls().iter().any(|(call, _)| call == "create"),
        "the unrelated app was created: {:?}",
        grill.calls()
    );
}

#[tokio::test]
async fn a_refusing_provider_never_puts_a_secret_in_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown, _grill) = test_agent_with_grill();
    // Wrong protocol version, with a value: the answer must be refused
    // without quoting it anywhere.
    agent.set_secret_providers(vault(
        &provider_script(
            dir.path(),
            &format!("printf '{{\"version\":9,\"value\":\"{SECRET}\"}}'"),
        ),
        PATIENT,
    ));
    let handle = tokio::spawn(async move { agent.run().await });

    let events = send_deploy(&tx, provider_app("api")).await;
    let instances = statuses(&tx).await;
    shutdown.cancel();
    handle.await.unwrap();

    assert!(
        !format!("{events:?}").contains(SECRET),
        "an event carried the secret: {events:?}"
    );
    let api = instances
        .iter()
        .find(|instance| instance.app_name == "api")
        .unwrap();
    let reason = api.reason.clone().unwrap_or_default();
    assert!(reason.contains("protocol version 9"), "{reason}");
    assert!(!format!("{instances:?}").contains(SECRET));
}

#[tokio::test]
async fn a_restart_asks_the_provider_again_and_fails_closed_when_it_stops_answering() {
    let dir = tempfile::tempdir().unwrap();
    let binary = answering(dir.path());
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&binary, PATIENT));
    expect_complete(&drain_deploy(&mut agent, provider_app("api")).await);
    let id = agent.supervisor.list_instances()[0].id.clone();

    // The provider breaks while the app runs, then the app exits.
    provider_script(dir.path(), "exit 1");
    grill.set_state(&id, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    assert_eq!(
        instance.launch_failure.as_deref(),
        Some("cannot resolve secret DB_PASSWORD: secret provider \"vault\" exited with status 1")
    );
    assert!(!instance.retry_pending, "a refused launch is not retried");
}

#[tokio::test]
async fn a_node_advertises_the_providers_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    agent.set_secret_providers(vault(&answering(dir.path()), PATIENT));
    let (capabilities, _) = agent.live_egress_report_state().await;
    let names: Vec<&str> = capabilities
        .secret_providers
        .iter()
        .map(|name| name.as_str())
        .collect();
    assert_eq!(names, ["vault"]);
}
