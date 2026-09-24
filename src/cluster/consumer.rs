//! Positive leader acknowledgements for durable consumer withdrawal receipts.

use super::ClusterHttp;
use std::io;

/// Submit an original generation using the enrolled node's authenticated transport.
/// Only a positive acknowledgement permits removing the local retry record.
pub async fn acknowledge(http: &ClusterHttp, leader: &str, generation: u64) -> io::Result<()> {
    if http.scheme() != "https" || !leader.starts_with("https://") {
        return Err(io::Error::other(
            "consumer receipts require authenticated HTTPS",
        ));
    }
    send_receipt(http, leader, generation).await
}

async fn send_receipt(http: &ClusterHttp, leader: &str, generation: u64) -> io::Result<()> {
    if generation == 0 {
        return Err(io::Error::other(
            "consumer receipt generation must be positive",
        ));
    }
    let url = format!("{leader}/v1/discovery/withdrawn");
    let receipt = crate::onion::withdrawal::EndpointWithdrawalReceipt {
        compatibility: crate::compatibility::CURRENT,
        generation,
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut request = http.client().post(&url).json(&receipt);
        if let Some(token) = http.bearer() {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(io::Error::other)?;
        if response.url().as_str() != url || response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(io::Error::other(format!(
                "consumer receipt is unconfirmed ({})",
                response.status()
            )));
        }
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("consumer receipt timed out; retry record retained"))?
}

/// Discharge endpoint consumers whose view lease has lapsed. Call it on the
/// leader only; anywhere else the writes are refused.
///
/// A consumer lapses when this leader hasn't served it for `silence` (in
/// production [`crate::onion::lease::CONSUMER_DISCHARGE_AFTER`]) and gossip
/// doesn't list it in `alive`. Its own lease ran out before that, so it has
/// stopped routing; the ledger can stop waiting for its receipts. Returns the
/// consumers whose discharge committed in this call.
pub async fn discharge_lapsed_consumers(
    council: &crate::council::CouncilNode,
    alive: &std::collections::HashSet<&str>,
    silence: std::time::Duration,
) -> Vec<String> {
    use crate::council::{CouncilResponse, RaftRequest};
    use std::time::Instant;

    let term = council.current_term();
    // A discharge whose outcome we never learnt (a timed-out write can still
    // commit) keeps its consumer unserved until a barrier settles it.
    let unsettled: Vec<String> = {
        let mut contacts = council.consumer_contacts().lock().await;
        contacts.observe_term(term, Instant::now());
        contacts.discharging().cloned().collect()
    };
    if !unsettled.is_empty() {
        let barrier =
            tokio::time::timeout(DISCHARGE_WRITE_TIMEOUT, council.write(RaftRequest::Noop)).await;
        if !matches!(barrier, Ok(Ok(_))) {
            return Vec::new();
        }
        let mut contacts = council.consumer_contacts().lock().await;
        for node in &unsettled {
            contacts.finish_discharge(node);
        }
    }

    let desired = council.desired_state().await;
    let lapsed = {
        let mut contacts = council.consumer_contacts().lock().await;
        let now = Instant::now();
        contacts.observe_term(term, now);
        let lapsed = contacts.lapsed(&desired.endpoint_consumers, alive, now, silence);
        for node in &lapsed {
            contacts.begin_discharge(node);
        }
        lapsed
    };

    let mut discharged = Vec::new();
    for node_id in lapsed {
        let written = tokio::time::timeout(
            DISCHARGE_WRITE_TIMEOUT,
            council.write(RaftRequest::DischargeEndpointConsumer {
                node_id: node_id.clone(),
            }),
        )
        .await;
        match written {
            Ok(Ok(CouncilResponse::Refused { reason })) => {
                eprintln!("scheduler: discharge of endpoint consumer {node_id} refused: {reason}");
                council
                    .consumer_contacts()
                    .lock()
                    .await
                    .finish_discharge(&node_id);
            }
            Ok(Ok(_)) => {
                eprintln!(
                    "scheduler: endpoint consumer {node_id} silent past its view lease; \
                     its withdrawal receipts are no longer awaited"
                );
                council
                    .consumer_contacts()
                    .lock()
                    .await
                    .finish_discharge(&node_id);
                discharged.push(node_id);
            }
            // Unknown outcome: stay unserved until a barrier settles it.
            _ => {}
        }
    }
    discharged
}

/// Upper bound on one discharge or barrier write.
const DISCHARGE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn consumer_receipt_retries_exact_generation_and_requires_positive_acknowledgement() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = attempts.clone();
        let app = Router::new().route(
            "/v1/discovery/withdrawn",
            post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(body): axum::Json<
                    crate::onion::withdrawal::EndpointWithdrawalReceipt,
                >| {
                    let attempts = captured.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer receipt-authority");
                        assert_eq!(body.generation, 17);
                        assert_eq!(body.compatibility, crate::compatibility::CURRENT);
                        match attempts.fetch_add(1, Ordering::SeqCst) {
                            0 => StatusCode::SERVICE_UNAVAILABLE,
                            1 => StatusCode::ACCEPTED,
                            2 => StatusCode::OK,
                            _ => StatusCode::NO_CONTENT,
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let leader = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = ClusterHttp::plaintext().with_bearer(Some("receipt-authority".into()));
        assert!(acknowledge(&http, &leader, 17).await.is_err());
        assert!(send_receipt(&http, &leader, 0).await.is_err());
        for _ in 0..3 {
            assert!(send_receipt(&http, &leader, 17).await.is_err());
        }
        send_receipt(&http, &leader, 17).await.unwrap();
        send_receipt(&http, &leader, 17).await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
        server.abort();
    }

    #[tokio::test]
    async fn consumer_receipt_redirect_does_not_confirm_original_leader() {
        let app = Router::new()
            .route(
                "/v1/discovery/withdrawn",
                post(|| async {
                    axum::response::Redirect::temporary("/elsewhere").into_response()
                }),
            )
            .route("/elsewhere", post(|| async { StatusCode::NO_CONTENT }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let leader = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert!(
            send_receipt(&ClusterHttp::plaintext(), &leader, 17)
                .await
                .is_err()
        );
        server.abort();
    }
}
