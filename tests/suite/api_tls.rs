//! The agent API served over mTLS.
//!
//! Proves the API server config and cluster HTTP client complete a mutually
//! authenticated HTTPS round trip with issued node identities.

use std::time::{Duration, SystemTime};

use axum::Router;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config, build_cluster_http_client};
use reliaburger::sesame::types::SerialNumber;

fn identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        node_id,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
    }
}

/// Serve a tiny router over TLS on an ephemeral port; return its address.
async fn spawn_tls_api(
    acceptor: tokio_rustls::TlsAcceptor,
    shutdown: CancellationToken,
    saw_client_certificate: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::net::SocketAddr {
    spawn_tls_router(
        acceptor,
        shutdown,
        saw_client_certificate,
        Router::new().route("/ping", get(|| async { "pong" })),
    )
    .await
}

async fn spawn_tls_router(
    acceptor: tokio_rustls::TlsAcceptor,
    shutdown: CancellationToken,
    saw_client_certificate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    router: Router,
) -> std::net::SocketAddr {
    use tower::Service;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut make_service = router.into_make_service();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                accepted = listener.accept() => {
                    let Ok((tcp, _)) = accepted else { continue };
                    let acceptor = acceptor.clone();
                    let saw_client_certificate = saw_client_certificate.clone();
                    let service = match make_service.call(()).await {
                        Ok(s) => s,
                        Err(infallible) => match infallible {},
                    };
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        saw_client_certificate.store(
                            tls.get_ref()
                                .1
                                .peer_certificates()
                                .is_some_and(|certificates| !certificates.is_empty()),
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        let svc = hyper_util::service::TowerToHyperService::new(service);
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                        .await;
                    });
                }
            }
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_api_round_trip_presents_the_calling_nodes_identity() {
    let hierarchy = ca::generate_ca_hierarchy("api-tls-test", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "node-01", 10);
    let client_id = identity(&hierarchy, "node-02", 11);

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
    );
    let shutdown = CancellationToken::new();
    let saw_client_certificate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let addr = spawn_tls_api(acceptor, shutdown.clone(), saw_client_certificate.clone()).await;

    let client = build_cluster_http_client(&client_id, CrlHandle::default()).unwrap();
    let resp = client
        .get(format!("https://{addr}/ping"))
        .send()
        .await
        .expect("HTTPS request should succeed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
    assert!(
        saw_client_certificate.load(std::sync::atomic::Ordering::SeqCst),
        "the peer API call completed without presenting its configured node identity"
    );

    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_does_not_trust_the_cluster_ca_is_refused() {
    let hierarchy = ca::generate_ca_hierarchy("api-tls-test", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "node-01", 10);

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
    );
    let shutdown = CancellationToken::new();
    let addr = spawn_tls_api(
        acceptor,
        shutdown.clone(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await;

    // A stock client doesn't trust our private CA, so the TLS handshake fails.
    let stock = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(
        stock
            .get(format!("https://{addr}/ping"))
            .send()
            .await
            .is_err(),
        "a client that doesn't trust the cluster CA must be refused"
    );

    shutdown.cancel();
}

#[cfg(feature = "ebpf")]
#[tokio::test]
async fn consumer_reconciler_retries_lost_receipts_after_tls_leader_change() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent, ClusterHandle};
    use reliaburger::cluster::orchestrate::{NodeAssignments, spawn_placement_reconciler};
    use reliaburger::onion::{
        catalog::{CatalogBackend, EndpointCatalog},
        service_id::ServiceId,
        withdrawal::{EndpointWithdrawalInstruction, EndpointWithdrawalReceipt, ServiceWithdrawal},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::{mpsc, oneshot, watch};
    let root = tempfile::tempdir().unwrap();
    let shutdown = CancellationToken::new();
    let hierarchy = ca::generate_ca_hierarchy("consumer-retry", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "leader", 10);
    let client_id = identity(&hierarchy, "consumer", 11);
    let http = reliaburger::cluster::ClusterHttp::secure(
        build_cluster_http_client(&client_id, CrlHandle::default()).unwrap(),
    );
    let catalog = EndpointCatalog::rebuild([(
        ServiceId::new("default", "remote"),
        8080,
        vec![CatalogBackend {
            execution: None,
            node_id: "producer".into(),
            node_ip: "192.0.2.3".parse().unwrap(),
            host_port: 30001,
            healthy: true,
        }],
    )])
    .unwrap();
    let instruction = EndpointWithdrawalInstruction {
        generation: 1,
        services: catalog
            .services
            .iter()
            .map(|(id, service)| {
                (
                    id.clone(),
                    ServiceWithdrawal {
                        service: service.clone(),
                        retire_vip: true,
                    },
                )
            })
            .collect(),
    };
    let deployment_started = Arc::new(AtomicBool::new(false));
    let assignments = NodeAssignments {
        endpoint_generation: 2,
        endpoint_withdrawals: vec![instruction],
        ..Default::default()
    };
    let mut addresses = vec![];
    let mut observed = vec![];
    let saw_certificate = Arc::new(AtomicBool::new(false));
    for lost_reply in [true, false] {
        let attempts = Arc::new(AtomicUsize::new(0));
        observed.push(attempts.clone());
        let assignments = assignments.clone();
        let started = deployment_started.clone();
        let initial_catalog = catalog.clone();
        let router = Router::new()
            .route("/v1/placements/consumer", get(move || {
                let mut assignments = assignments.clone();
                if !started.load(Ordering::SeqCst) {
                    assignments.endpoint_generation = 1;
                    assignments.endpoint_catalog = initial_catalog.clone();
                    assignments.endpoint_withdrawals.clear();
                }
                assignments.apps = vec![reliaburger::cluster::orchestrate::NodeAssignment {
                    name: "pending".into(), namespace: "default".into(), replicas: 1,
                    spec: reliaburger::config::Config::parse("[app.pending]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]").unwrap().app.remove("pending").unwrap(),
                }];
                async move { axum::Json(assignments) }
            }))
            .route("/v1/discovery/withdrawn", axum::routing::post(move |headers: axum::http::HeaderMap, axum::Json(receipt): axum::Json<EndpointWithdrawalReceipt>| {
                let attempts = attempts.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer service-authority");
                    assert_eq!(receipt.generation, 1);
                    assert_eq!(receipt.compatibility, reliaburger::compatibility::CURRENT);
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if lost_reply { tokio::time::sleep(Duration::from_secs(30)).await; }
                    axum::http::StatusCode::NO_CONTENT
                }
            }));
        let acceptor = tokio_rustls::TlsAcceptor::from(
            build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
        );
        addresses.push(
            spawn_tls_router(acceptor, shutdown.clone(), saw_certificate.clone(), router).await,
        );
    }
    let (_, membership_rx) = watch::channel(vec![]);
    let (_, snapshot_rx) = mpsc::channel(1);
    let (commands, receiver) = mpsc::channel(16);
    let grill = reliaburger::grill::mock::MockGrill::new();
    grill.set_launch_inventory(vec![]).await;
    let mut agent = BunAgent::with_cluster(
        grill,
        reliaburger::grill::port::PortAllocator::new(40000, 40100),
        receiver,
        shutdown.clone(),
        ClusterHandle {
            local_node_id: reliaburger::meat::NodeId::new("consumer"),
            membership_rx,
            raft_metrics_rx: None,
            council: None,
            snapshot_rx,
            wrapping_ikm: None,
            partition_blocklists: Default::default(),
            crl_handle: Default::default(),
        },
        "consumer".into(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent
        .recover_consumer_ownership(
            &root.path().join("discovery"),
            reliaburger::bun::consumer_owners::ConsumerIdentity {
                node_id: reliaburger::meat::NodeId::new("consumer"),
                cluster_identity: [42; 32],
            },
        )
        .await
        .unwrap();
    let agent = tokio::spawn(async move { agent.run().await });
    let (response, reply) = oneshot::channel();
    commands
        .send(AgentCommand::SyncClusterConsumer {
            generation: 1,
            catalog: Box::new(catalog),
            ingress: vec![],
            withdrawals: vec![],
            requested_at_ns: reliaburger::onion::lease::boot_clock_ns(),
            response,
        })
        .await
        .unwrap();
    assert!(reply.await.unwrap().unwrap().published);
    let (_, metrics) = watch::channel(openraft::RaftMetrics::new_initial(1));
    let directory = |index| reliaburger::mustard::directory::NodeDirectory {
        leader: Some(reliaburger::mustard::message::LeaderHint {
            node_id: reliaburger::meat::NodeId::new("leader"),
            term: index as u64 + 1,
            api_address: addresses[index],
            reporting_address: addresses[index],
        }),
        ..Default::default()
    };
    let (leaders, receiver) = watch::channel(directory(0));
    // Lose the first local confirmation too: the ready journal entry must be retried.
    let (forward, mut forwarded) = mpsc::channel(16);
    let confirms = Arc::new(AtomicUsize::new(0));
    let confirmations = confirms.clone();
    let forwarding = tokio::spawn(async move {
        let mut lost = false;
        let mut pending_deployment = None;
        while let Some(command) = forwarded.recv().await {
            if let AgentCommand::Deploy { events, .. } = command {
                assert!(pending_deployment.is_none(), "duplicate pending deployment");
                pending_deployment = Some(events);
                deployment_started.store(true, Ordering::SeqCst);
                continue;
            }
            if let AgentCommand::ConfirmConsumerReceipt {
                generation,
                response,
            } = command
            {
                if !lost {
                    lost = true;
                    confirmations.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                let (confirmed, reply) = oneshot::channel();
                commands
                    .send(AgentCommand::ConfirmConsumerReceipt {
                        generation,
                        response: confirmed,
                    })
                    .await
                    .unwrap();
                let result = reply.await.unwrap();
                assert!(result.is_ok());
                let _ = response.send(result);
                confirmations.fetch_add(1, Ordering::SeqCst);
            } else if commands.send(command).await.is_err() {
                break;
            }
        }
    });
    let reconciler = spawn_placement_reconciler(
        "consumer".into(),
        metrics,
        receiver,
        0,
        Some("service-authority".into()),
        forward,
        shutdown.clone(),
        http,
        Some(root.path().join("placements")),
        reliaburger::config::node::RuntimeSection::default().stop_confirmation_timeout(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while observed[0].load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            confirms.load(Ordering::SeqCst),
            0,
            "lost HTTP reply forgot local receipt"
        );
        leaders.send_replace(directory(1));
        while confirms.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    shutdown.cancel();
    reconciler.await.unwrap();
    forwarding.await.unwrap();
    agent.await.unwrap();
    assert!(saw_certificate.load(Ordering::SeqCst));
    assert!(observed[1].load(Ordering::SeqCst) >= 2);
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    assert!(
        journal
            .inventory()
            .consumer
            .as_ref()
            .unwrap()
            .receipts
            .is_empty()
    );
}
