//! Bounded, checksum-pinned downloads for managed cluster artefacts.

use std::{io::Read, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::upgrade::metadata::ReleaseMetadata;

/// Downloads artefacts with bounded requests and atomic, verified cache writes.
pub struct Downloader {
    client: reqwest::Client,
}

fn validate_url(url: &reqwest::Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        bail!("download URLs must not contain credentials");
    }
    let loopback = url.host_str().is_some_and(|host| {
        host.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(cfg!(test) && url.scheme() == "http" && loopback) {
        bail!("downloads require HTTPS");
    }
    Ok(())
}

impl Downloader {
    /// Apply this timeout to each complete request, including its response body.
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(15)))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("too many download redirects")
                } else if let Err(error) = validate_url(attempt.url()) {
                    attempt.error(error.to_string())
                } else {
                    attempt.follow()
                }
            }))
            .build()?;
        Ok(Self { client })
    }

    async fn response(&self, url: &str, limit: u64) -> Result<reqwest::Response> {
        let url = reqwest::Url::parse(url).context("invalid download URL")?;
        validate_url(&url)?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            bail!("download exceeds size limit");
        }
        Ok(response)
    }

    /// Fetch a release manifest, refusing unsupported schemas and oversized bodies.
    pub async fn metadata(&self, url: &str) -> Result<ReleaseMetadata> {
        const LIMIT: u64 = 1024 * 1024;
        let mut response = self.response(url, LIMIT).await?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            if body.len() as u64 + chunk.len() as u64 > LIMIT {
                bail!("release metadata exceeds size limit");
            }
            body.extend_from_slice(&chunk);
        }
        let metadata: ReleaseMetadata = serde_json::from_slice(&body)?;
        if metadata.schema != 1 {
            bail!("unsupported release metadata schema {}", metadata.schema);
        }
        Ok(metadata)
    }

    /// Reuse only a checksum-verified cache entry; publish new bytes atomically.
    pub async fn fetch(&self, url: &str, digest: &str, path: &Path, limit: u64) -> Result<()> {
        validate_url(&reqwest::Url::parse(url)?)?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid SHA-256 digest");
        }
        let expected = digest.to_ascii_lowercase();
        let cached_path = path.to_owned();
        let cached_digest = expected.clone();
        if tokio::task::spawn_blocking(move || -> Result<bool> {
            let mut file = match std::fs::File::open(&cached_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
                return Ok(false);
            }
            let mut hash = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            let mut total = 0_u64;
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                total += count as u64;
                if total > limit {
                    return Ok(false);
                }
                hash.update(&buffer[..count]);
            }
            Ok(format!("{:x}", hash.finalize()) == cached_digest)
        })
        .await??
        {
            return Ok(());
        }
        let mut response = self.response(url, limit).await?;
        let parent = path
            .parent()
            .context("download destination needs a parent directory")?
            .to_owned();
        let temporary = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new()
                .prefix(".download-")
                .tempfile_in(parent)
        })
        .await??;
        let handle = temporary.reopen()?;
        let mut output = tokio::fs::File::from_std(handle);
        let mut hash = Sha256::new();
        let mut total = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            total = total
                .checked_add(chunk.len() as u64)
                .context("download size overflow")?;
            if total > limit {
                bail!("download exceeds size limit");
            }
            hash.update(&chunk);
            output.write_all(&chunk).await?;
        }
        if format!("{:x}", hash.finalize()) != expected {
            bail!("download SHA-256 does not match release metadata");
        }
        output.sync_all().await?;
        drop(output);
        let destination = path.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let parent = destination
                .parent()
                .context("download destination needs a parent directory")?;
            temporary.persist(&destination)?;
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn server(
        body: &'static [u8],
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/asset",
            get(move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Body::from_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(body)]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/asset"), calls, task)
    }

    #[tokio::test]
    async fn stalled_body_times_out_without_publishing_a_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let app = Router::new().route(
            "/asset",
            get(|| async {
                Body::from_stream(futures_util::stream::pending::<
                    Result<&'static [u8], std::io::Error>,
                >())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let downloader = Downloader::new(Duration::from_millis(100)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            downloader.fetch(
                &format!("http://{address}/asset"),
                &"0".repeat(64),
                &path,
                1024,
            ),
        )
        .await;
        server.abort();
        assert!(result.unwrap().is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn unknown_metadata_schema_is_refused() {
        let (url, _, server) = server(br#"{"schema":2,"latest":"v0.1.0","releases":[]}"#).await;
        let downloader = Downloader::new(std::time::Duration::from_secs(2)).unwrap();
        assert!(downloader.metadata(&url).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn verified_download_is_reused_without_another_request() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let (url, calls, server) = server(b"verified bytes").await;
        let digest = crate::upgrade::signing::sha256_hex(b"verified bytes");
        let downloader = Downloader::new(std::time::Duration::from_secs(2)).unwrap();
        downloader.fetch(&url, &digest, &path, 1024).await.unwrap();
        server.abort();
        downloader.fetch(&url, &digest, &path, 1024).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"verified bytes");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_bad_download_cannot_replace_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        std::fs::write(&path, b"keep this").unwrap();
        let (url, _, server) = server(b"modified").await;
        let digest = crate::upgrade::signing::sha256_hex(b"expected");
        let downloader = Downloader::new(std::time::Duration::from_secs(2)).unwrap();
        assert!(downloader.fetch(&url, &digest, &path, 1024).await.is_err());
        server.abort();
        assert_eq!(std::fs::read(path).unwrap(), b"keep this");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn oversize_download_is_refused_before_becoming_visible() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let (url, _, server) = server(b"too many bytes").await;
        let digest = crate::upgrade::signing::sha256_hex(b"too many bytes");
        let downloader = Downloader::new(std::time::Duration::from_secs(2)).unwrap();
        assert!(downloader.fetch(&url, &digest, &path, 4).await.is_err());
        server.abort();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn remote_plaintext_download_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let downloader = Downloader::new(std::time::Duration::from_secs(2)).unwrap();
        assert!(
            downloader
                .fetch(
                    "http://192.0.2.1/asset",
                    &"0".repeat(64),
                    &root.path().join("asset"),
                    1024
                )
                .await
                .is_err()
        );
    }
}
