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
