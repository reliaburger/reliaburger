//! Resumable, checksum-pinned downloads for managed cluster artefacts.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::progress::Step;
use crate::upgrade::metadata::ReleaseMetadata;

/// Downloads artefacts with stall detection, resumable partial files and
/// atomic, verified cache writes.
pub struct Downloader {
    client: reqwest::Client,
    stall: Duration,
    release_mirror: Option<(String, reqwest::Url)>,
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
    /// Fail a request that sends no bytes for `stall`. There's deliberately
    /// no limit on a transfer that keeps making progress: a slow link is
    /// not a broken one, and the caller owns the overall deadline.
    pub fn new(stall: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(stall.min(Duration::from_secs(15)))
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
        Ok(Self {
            client,
            stall,
            release_mirror: None,
        })
    }

    /// Fetch this version's release assets from an explicit HTTPS directory.
    /// Checksums, signatures, deadlines and unrelated tooling URLs are unchanged.
    pub fn with_release_mirror(
        mut self,
        version: &crate::upgrade::BinaryVersion,
        mirror: &str,
    ) -> Result<Self> {
        let mut base = reqwest::Url::parse(mirror).context("invalid release mirror URL")?;
        validate_url(&base)?;
        if base.query().is_some() || base.fragment().is_some() {
            bail!("release mirror URL must not contain a query or fragment");
        }
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        let original =
            format!("https://github.com/reliaburger/reliaburger/releases/download/{version}/");
        self.release_mirror = Some((original, base));
        Ok(self)
    }

    fn download_url(&self, url: &str) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(url).context("invalid download URL")?;
        validate_url(&url)?;
        if let Some((original, mirror)) = &self.release_mirror
            && let Some(name) = url.as_str().strip_prefix(original)
        {
            if name.is_empty()
                || name == "."
                || name == ".."
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            {
                bail!("mirrored release asset must have one plain filename");
            }
            return Ok(mirror.join(name)?);
        }
        Ok(url)
    }

    async fn response(&self, url: &str, limit: u64, from: u64) -> Result<reqwest::Response> {
        let url = self.download_url(url)?;
        let mut request = self.client.get(url);
        if from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        let response = tokio::time::timeout(self.stall, request.send())
            .await
            .context("download server did not answer")?
            .map_err(reqwest::Error::without_url)?;
        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(response);
        }
        let response = response
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

    /// The next body chunk, or an error if none arrives within the stall limit.
    async fn chunk(&self, response: &mut reqwest::Response) -> Result<Option<hyper::body::Bytes>> {
        let chunk = tokio::time::timeout(self.stall, response.chunk())
            .await
            .with_context(|| {
                format!(
                    "download stalled: no data for {}s; re-run to resume it",
                    self.stall.as_secs()
                )
            })?
            .map_err(reqwest::Error::without_url)?;
        Ok(chunk)
    }

    /// Fetch a small JSON release document (at most 1 MiB) and parse it.
    /// Nothing about its contents is trusted yet: callers verify it.
    pub async fn json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        const LIMIT: u64 = 1024 * 1024;
        let mut response = self.response(url, LIMIT, 0).await?;
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk(&mut response).await? {
            if body.len() as u64 + chunk.len() as u64 > LIMIT {
                bail!("release metadata exceeds size limit");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body)?)
    }

    /// Fetch a release manifest, refusing unsupported schemas and oversized bodies.
    pub async fn metadata(&self, url: &str) -> Result<ReleaseMetadata> {
        let metadata: ReleaseMetadata = self.json(url).await?;
        if metadata.schema != 1 {
            bail!("unsupported release metadata schema {}", metadata.schema);
        }
        Ok(metadata)
    }

    /// Reuse only a checksum-verified cache entry; publish new bytes atomically.
    ///
    /// Bytes arrive in `<path>.partial`. If a previous run was interrupted,
    /// that file's bytes are kept and the rest is requested with an HTTP
    /// `Range` header. The SHA-256 of the whole file decides whether it is
    /// published, so a resumed file is trusted exactly as much as a fresh one.
    pub async fn fetch(
        &self,
        url: &str,
        digest: &str,
        path: &Path,
        limit: u64,
        step: Option<&Step>,
    ) -> Result<()> {
        self.download_url(url)?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid SHA-256 digest");
        }
        let expected = digest.to_ascii_lowercase();
        if let Some((hash, _)) = hash_file(path.to_owned(), limit).await?
            && format!("{:x}", hash.finalize()) == expected
        {
            if let Some(step) = step {
                step.note("cached");
            }
            return Ok(());
        }
        let partial = partial_path(path)?;
        let result = self.transfer(url, &expected, &partial, limit, step).await;
        if result.is_err() {
            // Keep a partial worth resuming; drop one that can never succeed.
            let keep = matches!(tokio::fs::metadata(&partial).await, Ok(meta) if meta.len() > 0)
                && result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.downcast_ref::<Unrecoverable>().is_none());
            if !keep {
                let _ = tokio::fs::remove_file(&partial).await;
            }
            return result;
        }
        let destination = path.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let parent = destination
                .parent()
                .context("download destination needs a parent directory")?;
            std::fs::rename(&partial, &destination)?;
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// Fill `partial` until it holds the complete, verified file.
    async fn transfer(
        &self,
        url: &str,
        expected: &str,
        partial: &Path,
        limit: u64,
        step: Option<&Step>,
    ) -> Result<()> {
        let (mut hash, mut total) = hash_file(partial.to_owned(), limit)
            .await?
            .unwrap_or_default();
        if total > 0 && format!("{:x}", hash.clone().finalize()) == expected {
            // Interrupted after the last byte but before publishing.
            if let Some(step) = step {
                step.note("resumed");
            }
            return Ok(());
        }
        let mut response = self.response(url, limit, total).await?;
        let resumed = total > 0
            && response.status() == reqwest::StatusCode::PARTIAL_CONTENT
            && content_range_start(&response) == Some(total);
        if !resumed {
            // No partial, a server that ignored the range, or a partial that
            // is already complete or longer than the file: start again.
            if response.status() != reqwest::StatusCode::OK {
                response = self.response(url, limit, 0).await?;
            }
            (hash, total) = (Sha256::new(), 0);
        }
        let length = response.content_length().map(|length| length + total);
        if let Some(step) = step {
            step.begin_transfer(total, length);
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true);
        if resumed {
            options.append(true);
        } else {
            options.truncate(true);
        }
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(partial).await?;
        while let Some(chunk) = self.chunk(&mut response).await? {
            total = total
                .checked_add(chunk.len() as u64)
                .context("download size overflow")?;
            if total > limit {
                return Err(Unrecoverable("download exceeds size limit").into());
            }
            hash.update(&chunk);
            output.write_all(&chunk).await?;
            if let Some(step) = step {
                step.add_bytes(chunk.len() as u64);
            }
        }
        output.sync_all().await?;
        if format!("{:x}", hash.finalize()) != expected {
            return Err(Unrecoverable("download SHA-256 does not match release metadata").into());
        }
        Ok(())
    }
}

/// A failure that retrying from the same partial file cannot fix.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Unrecoverable(&'static str);

fn partial_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .context("download destination needs a file name")?;
    let mut partial = name.to_owned();
    partial.push(".partial");
    Ok(path.with_file_name(partial))
}

/// The start offset of a `206 Partial Content` response.
fn content_range_start(response: &reqwest::Response) -> Option<u64> {
    let range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let (start, _) = range.strip_prefix("bytes ")?.split_once('-')?;
    start.parse().ok()
}

/// Hash an existing regular file of at most `limit` bytes, off the runtime.
/// Returns `None` when there's no such file, or it's too large to be ours.
async fn hash_file(path: PathBuf, limit: u64) -> Result<Option<(Sha256, u64)>> {
    tokio::task::spawn_blocking(move || -> Result<Option<(Sha256, u64)>> {
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
            return Ok(None);
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 256 * 1024];
        let mut total = 0_u64;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > limit {
                return Ok(None);
            }
            hash.update(&buffer[..count]);
        }
        Ok(Some((hash, total)))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use futures_util::StreamExt;
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
    async fn release_mirror_fetches_exact_candidate_bytes_and_retains_checksums() {
        let (mirror, calls, task) = server(b"candidate bytes").await;
        let downloader = Downloader::new(Duration::from_secs(2))
            .unwrap()
            .with_release_mirror(&"v0.1.0".parse().unwrap(), mirror.trim_end_matches("asset"))
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("download");
        let original = "https://github.com/reliaburger/reliaburger/releases/download/v0.1.0/asset";
        let digest = format!("{:x}", Sha256::digest(b"candidate bytes"));
        downloader
            .fetch(original, &digest, &path, 1024, None)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"candidate bytes");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            downloader
                .fetch(original, &"0".repeat(64), &path, 1024, None)
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"candidate bytes");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn release_mirror_fetches_metadata_without_rewriting_unrelated_downloads() {
        let (mirror, calls, task) =
            server(br#"{"schema":1,"latest":"v0.1.0","releases":[]}"#).await;
        let downloader = Downloader::new(Duration::from_secs(2))
            .unwrap()
            .with_release_mirror(&"v0.1.0".parse().unwrap(), mirror.trim_end_matches("asset"))
            .unwrap();
        let metadata = downloader
            .metadata("https://github.com/reliaburger/reliaburger/releases/download/v0.1.0/asset")
            .await
            .unwrap();
        assert_eq!(metadata.schema, 1);
        let (other, other_calls, other_task) = server(b"separate tooling").await;
        let root = tempfile::tempdir().unwrap();
        downloader
            .fetch(
                &other,
                &format!("{:x}", Sha256::digest(b"separate tooling")),
                &root.path().join("tool"),
                1024,
                None,
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 1);
        task.abort();
        other_task.abort();
    }

    #[test]
    fn release_mirror_refuses_ambiguous_or_insecure_bases() {
        for mirror in [
            "http://example.com/",
            "file:///tmp/",
            "relative/path",
            "https://user:password@example.com/",
            "https://example.com/?token=secret",
            "https://example.com/#fragment",
        ] {
            assert!(
                Downloader::new(Duration::from_secs(2))
                    .unwrap()
                    .with_release_mirror(&"v0.1.0".parse().unwrap(), mirror)
                    .is_err(),
                "accepted {mirror}"
            );
        }
    }

    /// Serves `body`, honouring `Range: bytes=N-` unless `ignore_ranges`,
    /// and records every Range header it receives.
    async fn range_server(
        body: &'static [u8],
        ignore_ranges: bool,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<Option<String>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::http::{HeaderMap, StatusCode, header};
        use axum::response::IntoResponse;
        let ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = ranges.clone();
        let app = Router::new().route(
            "/asset",
            get(move |headers: HeaderMap| async move {
                let range = headers
                    .get(header::RANGE)
                    .map(|value| value.to_str().unwrap().to_owned());
                seen.lock().unwrap().push(range.clone());
                let start = range
                    .and_then(|range| {
                        range
                            .strip_prefix("bytes=")?
                            .strip_suffix('-')?
                            .parse::<usize>()
                            .ok()
                    })
                    .filter(|_| !ignore_ranges);
                match start {
                    Some(start) if start >= body.len() => {
                        StatusCode::RANGE_NOT_SATISFIABLE.into_response()
                    }
                    Some(start) => (
                        StatusCode::PARTIAL_CONTENT,
                        [(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{}/{}", body.len() - 1, body.len()),
                        )],
                        &body[start..],
                    )
                        .into_response(),
                    None => body.into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/asset"), ranges, task)
    }

    const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    #[tokio::test]
    async fn interrupted_download_resumes_from_its_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        std::fs::write(root.path().join("asset.partial"), &BODY[..10]).unwrap();
        let (url, ranges, server) = range_server(BODY, false).await;
        let downloader = Downloader::new(Duration::from_secs(2)).unwrap();
        let digest = crate::upgrade::signing::sha256_hex(BODY);
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(*ranges.lock().unwrap(), vec![Some("bytes=10-".to_owned())]);
        assert!(!root.path().join("asset.partial").exists());
    }

    #[tokio::test]
    async fn a_server_that_ignores_ranges_gets_a_clean_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        std::fs::write(root.path().join("asset.partial"), &BODY[..10]).unwrap();
        let (url, _, server) = range_server(BODY, true).await;
        let downloader = Downloader::new(Duration::from_secs(2)).unwrap();
        let digest = crate::upgrade::signing::sha256_hex(BODY);
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
    }

    #[tokio::test]
    async fn a_corrupt_partial_is_discarded_so_the_next_run_starts_clean() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let partial = root.path().join("asset.partial");
        std::fs::write(&partial, b"XXXXXXXXXX").unwrap();
        let (url, ranges, server) = range_server(BODY, false).await;
        let downloader = Downloader::new(Duration::from_secs(2)).unwrap();
        let digest = crate::upgrade::signing::sha256_hex(BODY);
        assert!(
            downloader
                .fetch(&url, &digest, &path, 1024, None)
                .await
                .is_err()
        );
        assert!(!partial.exists());
        assert!(!path.exists());
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(
            *ranges.lock().unwrap(),
            vec![Some("bytes=10-".to_owned()), None]
        );
    }

    #[tokio::test]
    async fn a_complete_partial_is_published_without_another_request() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        std::fs::write(root.path().join("asset.partial"), BODY).unwrap();
        let (url, ranges, server) = range_server(BODY, false).await;
        let downloader = Downloader::new(Duration::from_secs(2)).unwrap();
        let digest = crate::upgrade::signing::sha256_hex(BODY);
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert!(ranges.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn slow_but_steady_downloads_are_not_cut_off() {
        // Each chunk arrives well inside the stall limit, but the whole body
        // takes several times longer than it; the old whole-request timeout
        // would have failed this.
        let app = Router::new().route(
            "/asset",
            get(|| async {
                let chunks = futures_util::stream::iter(BODY.chunks(6)).then(|chunk| async move {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Ok::<_, std::io::Error>(chunk)
                });
                Body::from_stream(chunks)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let progress = super::super::progress::tests_support::detached_step();
        let downloader = Downloader::new(Duration::from_millis(150)).unwrap();
        downloader
            .fetch(
                &format!("http://{address}/asset"),
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                Some(&progress),
            )
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(
            super::super::progress::tests_support::bytes(&progress),
            BODY.len() as u64
        );
    }

    #[tokio::test]
    async fn a_stalled_transfer_keeps_its_bytes_for_the_next_run() {
        let app = Router::new().route(
            "/asset",
            get(|| async {
                let first = futures_util::stream::iter([Ok::<_, std::io::Error>(&BODY[..10])]);
                Body::from_stream(first.chain(futures_util::stream::pending()))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let downloader = Downloader::new(Duration::from_millis(100)).unwrap();
        let error = downloader
            .fetch(
                &format!("http://{address}/asset"),
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("stalled"), "{error}");
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(root.path().join("asset.partial")).unwrap(),
            &BODY[..10]
        );
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
                None,
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
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
        server.abort();
        downloader
            .fetch(&url, &digest, &path, 1024, None)
            .await
            .unwrap();
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
        assert!(
            downloader
                .fetch(&url, &digest, &path, 1024, None)
                .await
                .is_err()
        );
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
        assert!(
            downloader
                .fetch(&url, &digest, &path, 4, None)
                .await
                .is_err()
        );
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
                    1024,
                    None
                )
                .await
                .is_err()
        );
    }
}
