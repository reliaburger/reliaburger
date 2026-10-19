//! An instance whose retirement is proven leaves no runtime metadata (#738).
//!
//! Deleting an app, scaling it down or replacing its instances in a rollout
//! asks the runtime to forget each retired identity: its ownership journal,
//! its route and their locks. A plain `relish stop` keeps its replicas owned,
//! so it forgets nothing.

use super::*;

/// The `forget_retired` calls the runtime has seen so far.
fn forgotten(grill: &MockGrill) -> Vec<InstanceId> {
    grill
        .calls()
        .into_iter()
        .filter(|(operation, _)| operation == "forget_retired")
        .map(|(_, id)| id)
        .collect()
}

/// Wait until the runtime has been asked to forget every one of `ids`.
async fn wait_until_forgotten(grill: &MockGrill, ids: &[InstanceId]) {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !ids.iter().all(|id| forgotten(grill).contains(id)) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "runtime never forgot {ids:?}; forgotten: {:?}",
        forgotten(grill)
    );
}

async fn instance_ids(tx: &mpsc::Sender<AgentCommand>) -> Vec<InstanceId> {
    let (status, _) = timed_status(tx).await;
    let mut ids: Vec<_> = status.into_iter().map(|i| InstanceId(i.id)).collect();
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    ids
}

async fn send_retire(tx: &mpsc::Sender<AgentCommand>, app_name: &str) -> Result<(), BunError> {
    let (response, reply) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: app_name.into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), reply)
        .await
        .expect("retirement never finished")
        .unwrap()
}

#[tokio::test]
async fn deleting_an_app_forgets_the_runtime_metadata_of_every_instance() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nreplicas = 2\n").unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    let ids = instance_ids(&tx).await;
    assert_eq!(ids.len(), 2);
    assert!(forgotten(&grill).is_empty());

    send_retire(&tx, "web").await.unwrap();
    wait_until_forgotten(&grill, &ids).await;

    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn a_plain_stop_keeps_the_runtime_metadata_of_its_owned_replicas() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
    expect_complete(&send_deploy(&tx, config).await);

    tokio::time::timeout(std::time::Duration::from_secs(10), send_stop(&tx, "web"))
        .await
        .expect("stop never finished")
        .unwrap()
        .unwrap();
    // The stopped replica stays owned; a later apply rolls over it.
    assert_eq!(instance_ids(&tx).await.len(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(forgotten(&grill).is_empty(), "{:?}", forgotten(&grill));

    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn scaling_down_forgets_the_retired_instances_but_not_the_one_it_keeps() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nreplicas = 3\n").unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    let before = instance_ids(&tx).await;

    let config = Config::parse("[app.web]\nimage = 'web:v1'\nreplicas = 1\n").unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    let after = instance_ids(&tx).await;
    assert_eq!(after.len(), 1);
    let retired: Vec<_> = before
        .iter()
        .filter(|id| !after.contains(id))
        .cloned()
        .collect();
    assert!(!retired.is_empty());

    wait_until_forgotten(&grill, &retired).await;
    assert!(
        after.iter().all(|id| !forgotten(&grill).contains(id)),
        "a kept instance was forgotten: {:?}",
        forgotten(&grill)
    );

    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn a_rollout_forgets_the_instances_it_replaced_but_not_their_replacements() {
    for strategy in ["rolling", "blue-green"] {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        let config = Config::parse("[app.web]\nimage = 'web:v1'\nreplicas = 2\n").unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        let old = instance_ids(&tx).await;

        let config = Config::parse(&format!(
            "[app.web]\nimage = 'web:v2'\nreplicas = 2\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n"
        ))
        .unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        let new = instance_ids(&tx).await;
        assert!(
            old.iter().all(|id| !new.contains(id)),
            "{strategy}: {new:?}"
        );

        wait_until_forgotten(&grill, &old).await;
        assert!(
            new.iter().all(|id| !forgotten(&grill).contains(id)),
            "{strategy}: a replacement was forgotten: {:?}",
            forgotten(&grill)
        );

        shutdown.cancel();
        task.await.unwrap();
    }
}

/// The generation-checked forgets the runtime has seen so far (#850).
fn generation_forgets(grill: &MockGrill) -> Vec<InstanceId> {
    grill
        .calls()
        .into_iter()
        .filter(|(operation, _)| operation == "forget_retired_generation")
        .map(|(_, id)| id)
        .collect()
}

/// Wait until the runtime has been asked to forget a generation of `id`.
async fn wait_until_generation_forgotten(grill: &MockGrill, id: &InstanceId) {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !generation_forgets(grill).contains(id) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "runtime never forgot {id:?}; forgotten: {:?}",
        generation_forgets(grill)
    );
}

#[tokio::test]
async fn deleting_an_app_forgets_its_init_containers_journals() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let init = InstanceId("default__web-0__init-0".into());
    grill.set_state(&init, ContainerState::Stopped);
    grill.set_exit_code(&init, Some(0));
    let task = tokio::spawn(async move { agent.run().await });
    expect_complete(&send_deploy(&tx, config_with_init_container()).await);
    assert_eq!(
        instance_ids(&tx).await,
        [InstanceId("default__web-0".into())]
    );
    assert!(grill.has_launch(&init), "the initialiser never launched");

    send_retire(&tx, "web").await.unwrap();
    wait_until_generation_forgotten(&grill, &init).await;
    assert!(
        !grill.has_launch(&init),
        "the deleted app's initialiser journal was kept"
    );

    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn an_immediate_redeploy_still_reads_its_own_init_containers_exit_code() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let init = InstanceId("default__web-0__init-0".into());
    grill.set_state(&init, ContainerState::Stopped);
    grill.set_exit_code(&init, Some(0));
    let task = tokio::spawn(async move { agent.run().await });
    expect_complete(&send_deploy(&tx, config_with_init_container()).await);

    // The forget the deletion asks for lands late: after the redeploy has
    // created its own initialiser under the same identity, just before the
    // redeploy reads that initialiser's exit code.
    grill.park_forgets();
    send_retire(&tx, "web").await.unwrap();
    wait_until_generation_forgotten(&grill, &init).await;
    expect_complete(&send_deploy(&tx, config_with_init_container()).await);
    assert_eq!(
        instance_ids(&tx).await,
        [InstanceId("default__web-0".into())]
    );
    assert!(
        grill
            .calls()
            .contains(&("forget_refused".to_string(), init.clone())),
        "the late forget never met the successor: {:?}",
        grill.calls()
    );
    assert!(
        grill.has_launch(&init),
        "the late forget removed the successor's journal"
    );

    shutdown.cancel();
    task.await.unwrap();
}
