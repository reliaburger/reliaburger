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

/// Attempts in a row that may fail without receiving a new byte before a
/// download gives up. Any attempt that gets further than the last one
/// refills the budget, so a flaky but working link always finishes.
const ATTEMPTS: u32 = 10;

/// Wait before the first retry; it doubles with each fruitless attempt.
const BACKOFF: Duration = Duration::from_secs(1);

/// The longest wait between two attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Downloads artefacts with stall detection, in-run retries that resume
/// where the connection dropped, and atomic, verified cache writes.
pub struct Downloader {
    client: reqwest::Client,
    stall: Duration,
    release_mirror: Option<(String, reqwest::Url)>,
    attempts: u32,
    backoff: Duration,
}

/// A failure the network might not repeat: a dropped or stalled
/// connection, or a server that's busy or briefly broken. Anything else,
/// such as a 404, a digest mismatch or a full disk, fails straight away.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct Transient(anyhow::Error);

impl Transient {
    fn from_reqwest(error: reqwest::Error) -> anyhow::Error {
        // Redirect and builder errors are policy, not weather.
        let transient = !error.is_redirect() && !error.is_builder();
        // `without_url` keeps a redirect's signed query out of the message.
        let error = anyhow::Error::new(error.without_url());
        if transient {
            Transient(error).into()
        } else {
            error
        }
    }
}

fn is_transient(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Transient>().is_some()
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
    /// Fail an attempt that sends no bytes for `stall`, then retry it.
    /// There's deliberately no limit on a transfer that keeps making
    /// progress: a slow link is not a broken one, and the caller owns the
    /// overall deadline.
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
            attempts: ATTEMPTS,
            backoff: BACKOFF,
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

    /// How long to wait after `failures` fruitless attempts in a row.
    fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.backoff.saturating_mul(1 << doublings).min(MAX_BACKOFF)
    }

    /// Request `url` from byte `from`. Every call starts at the original
    /// URL, so a retry follows a fresh redirect rather than a signed,
    /// short-lived one that may have expired since.
    async fn response(&self, url: &str, limit: u64, from: u64) -> Result<reqwest::Response> {
        let url = self.download_url(url)?;
        let mut request = self.client.get(url);
        if from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        let response = tokio::time::timeout(self.stall, request.send())
            .await
            .map_err(|_| {
                Transient(anyhow::anyhow!(
                    "download server did not answer within {}s",
                    self.stall.as_secs()
                ))
            })?
            .map_err(Transient::from_reqwest)?;
        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(response);
        }
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            return Err(Transient(anyhow::anyhow!("download server answered {status}")).into());
        }
        if !status.is_success() {
            bail!("download server answered {status}");
        }
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
            .map_err(|_| {
                Transient(anyhow::anyhow!(
                    "download stalled: no data for {}s",
                    self.stall.as_secs_f64()
                ))
            })?
            .map_err(Transient::from_reqwest)?;
        Ok(chunk)
    }

    /// Fetch a small JSON release document (at most 1 MiB) and parse it,
    /// retrying the whole request when the network lets it down.
    /// Nothing about its contents is trusted yet: callers verify it.
    pub async fn json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut failures = 0;
        loop {
            match self.json_once(url).await {
                Err(error) if is_transient(&error) && failures + 1 < self.attempts => {
                    failures += 1;
                    tokio::time::sleep(self.delay(failures)).await;
                }
                result => return result,
            }
        }
    }

    async fn json_once<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
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
    /// Bytes arrive in `<path>.partial`. When a connection drops or stalls,
    /// the download carries on from that file's length with an HTTP
    /// `Range` header, in this run and, if this run gives up, in the next
    /// one. The SHA-256 of the whole file decides whether it is published,
    /// so a resumed file is trusted exactly as much as a fresh one.
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

    /// Fill `partial` until it holds the complete, verified file, retrying
    /// dropped connections from wherever the file has got to.
    async fn transfer(
        &self,
        url: &str,
        expected: &str,
        partial: &Path,
        limit: u64,
        step: Option<&Step>,
    ) -> Result<()> {
        let mut file = Partial {
            path: partial,
            hash: Sha256::new(),
            length: 0,
        };
        if let Some((hash, length)) = hash_file(partial.to_owned(), limit).await? {
            (file.hash, file.length) = (hash, length);
        }
        // The furthest any attempt has got. Only beating it counts as
        // progress, so a server that ignores ranges and drops at the same
        // byte every time still runs out of attempts.
        let mut furthest = file.length;
        let mut failures = 0;
        let mut attempt = 0;
        loop {
            if file.length > 0 && format!("{:x}", file.hash.clone().finalize()) == expected {
                // Everything arrived before the connection went, perhaps in
                // an earlier run that stopped before publishing.
                if attempt == 0
                    && let Some(step) = step
                {
                    step.note("resumed");
                }
                return Ok(());
            }
            let error = match self.attempt(url, &mut file, limit, step, attempt).await {
                Ok(()) => break,
                Err(error) if is_transient(&error) => error,
                Err(error) => return Err(error),
            };
            if file.length > furthest {
                furthest = file.length;
                failures = 0;
            }
            failures += 1;
            if failures >= self.attempts {
                bail!(
                    "download failed {failures} attempts in a row without receiving new data \
                     (last error: {error:#}); the partial file is kept, so a re-run resumes it"
                );
            }
            tokio::time::sleep(self.delay(failures)).await;
            attempt += 1;
        }
        if format!("{:x}", file.hash.finalize()) != expected {
            return Err(Unrecoverable("download SHA-256 does not match release metadata").into());
        }
        Ok(())
    }

    /// One request: append what the server sends to `file` until the body
    /// ends or the connection fails. Retry `attempt` 0 is the first.
    async fn attempt(
        &self,
        url: &str,
        file: &mut Partial<'_>,
        limit: u64,
        step: Option<&Step>,
        attempt: u32,
    ) -> Result<()> {
        let mut response = self.response(url, limit, file.length).await?;
        let resumed = file.length > 0
            && response.status() == reqwest::StatusCode::PARTIAL_CONTENT
            && content_range_start(&response) == Some(file.length);
        if !resumed {
            // No partial, a server that ignored the range, or a partial that
            // is already complete or longer than the file: start again.
            if response.status() != reqwest::StatusCode::OK {
                response = self.response(url, limit, 0).await?;
            }
            (file.hash, file.length) = (Sha256::new(), 0);
        }
        let length = response.content_length().map(|length| length + file.length);
        if let Some(step) = step {
            if attempt == 0 {
                step.begin_transfer(file.length, length);
            } else {
                step.retry(attempt, file.length, length);
            }
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
        let mut output = options.open(file.path).await?;
        while let Some(chunk) = self.chunk(&mut response).await? {
            let length = file
                .length
                .checked_add(chunk.len() as u64)
                .context("download size overflow")?;
            if length > limit {
                return Err(Unrecoverable("download exceeds size limit").into());
            }
            // The hash and length only move once the bytes are written, so
            // they always describe what's on disk when a retry resumes.
            output.write_all(&chunk).await?;
            file.hash.update(&chunk);
            file.length = length;
            if let Some(step) = step {
                step.add_bytes(chunk.len() as u64);
            }
        }
        output.sync_all().await?;
        Ok(())
    }
}

/// The partial file being filled, with the running hash of its contents.
struct Partial<'a> {
    path: &'a Path,
    hash: Sha256,
    length: u64,
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
        let downloader = retrying(Duration::from_millis(100), 2);
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
        let downloader = retrying(Duration::from_millis(100), 2);
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

    /// What the scripted server does with one request.
    #[derive(Debug, Clone, Copy)]
    enum Reply {
        /// Send the body from the requested offset (or from 0 when
        /// `honour_range` is false), stopping after `send` bytes: closing
        /// the connection, or holding it open silently when `stall`.
        Body {
            honour_range: bool,
            send: Option<usize>,
            stall: bool,
        },
        /// Answer with just this status.
        Status(u16),
        /// Redirect to this path.
        Redirect(&'static str),
    }

    const WHOLE: Reply = Reply::Body {
        honour_range: true,
        send: None,
        stall: false,
    };

    fn cut(send: usize) -> Reply {
        Reply::Body {
            honour_range: true,
            send: Some(send),
            stall: false,
        }
    }

    type Requests = Arc<std::sync::Mutex<Vec<(String, Option<u64>)>>>;

    /// A raw HTTP/1.1 server whose `script` decides each reply from the
    /// request number, path and `Range` start, so tests can drop a
    /// connection mid-body the way a flaky link does: a `Content-Length`
    /// that the body never reaches.
    async fn scripted_server(
        body: &'static [u8],
        script: impl Fn(usize, &str, Option<u64>) -> Reply + Send + Sync + 'static,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let requests: Requests = Arc::default();
        let seen = requests.clone();
        let script = Arc::new(script);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let (seen, script) = (seen.clone(), script.clone());
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    let mut request_line = String::new();
                    socket.read_line(&mut request_line).await.unwrap();
                    let path = request_line.split(' ').nth(1).unwrap_or("/").to_owned();
                    let mut start = None;
                    loop {
                        let mut header = String::new();
                        socket.read_line(&mut header).await.unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(value) = header.to_ascii_lowercase().strip_prefix("range:") {
                            start = value
                                .trim()
                                .strip_prefix("bytes=")
                                .and_then(|range| range.strip_suffix('-'))
                                .and_then(|offset| offset.parse::<u64>().ok());
                        }
                    }
                    let number = {
                        let mut seen = seen.lock().unwrap();
                        seen.push((path.clone(), start));
                        seen.len()
                    };
                    let mut socket = socket.into_inner();
                    let reply = script(number, &path, start);
                    let head = match reply {
                        Reply::Status(code) => {
                            format!("HTTP/1.1 {code} Scripted\r\ncontent-length: 0\r\n\r\n")
                        }
                        Reply::Redirect(to) => format!(
                            "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\n\r\n"
                        ),
                        Reply::Body { honour_range, .. } => match start {
                            Some(start) if honour_range => format!(
                                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{}/{}\r\ncontent-length: {}\r\n\r\n",
                                body.len() - 1,
                                body.len(),
                                body.len() - start as usize
                            ),
                            _ => {
                                format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len())
                            }
                        },
                    };
                    socket.write_all(head.as_bytes()).await.unwrap();
                    if let Reply::Body {
                        honour_range,
                        send,
                        stall,
                    } = reply
                    {
                        let from = if honour_range {
                            start.unwrap_or(0) as usize
                        } else {
                            0
                        };
                        let rest = &body[from..];
                        let count = send.unwrap_or(rest.len()).min(rest.len());
                        let _ = socket.write_all(&rest[..count]).await;
                        let _ = socket.flush().await;
                        if stall {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }
                });
            }
        });
        (format!("http://{address}/asset"), requests, task)
    }

    /// A downloader with fast retries, so the tests don't sit in backoff.
    fn retrying(stall: Duration, attempts: u32) -> Downloader {
        let mut downloader = Downloader::new(stall).unwrap();
        downloader.attempts = attempts;
        downloader.backoff = Duration::from_millis(5);
        downloader
    }

    fn starts(requests: &Requests) -> Vec<Option<u64>> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, start)| *start)
            .collect()
    }

    #[tokio::test]
    async fn dropped_connections_are_resumed_within_the_same_run() {
        // Every connection dies after 7 bytes: the link that kept killing
        // quickstart's guest image download.
        let (url, requests, server) = scripted_server(BODY, |_, _, _| cut(7)).await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let progress = super::super::progress::tests_support::detached_step();
        retrying(Duration::from_secs(2), 3)
            .fetch(
                &url,
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
            starts(&requests),
            vec![None, Some(7), Some(14), Some(21), Some(28), Some(35)]
        );
        assert_eq!(
            super::super::progress::tests_support::bytes(&progress),
            BODY.len() as u64
        );
        assert_eq!(super::super::progress::tests_support::retries(&progress), 5);
        assert!(!root.path().join("asset.partial").exists());
    }

    #[tokio::test]
    async fn a_stalled_attempt_is_retried_from_where_it_stopped() {
        let (url, requests, server) = scripted_server(BODY, |number, _, _| match number {
            1 => Reply::Body {
                honour_range: true,
                send: Some(12),
                stall: true,
            },
            _ => WHOLE,
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        retrying(Duration::from_millis(150), 3)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(starts(&requests), vec![None, Some(12)]);
    }

    #[tokio::test]
    async fn a_retry_the_server_answers_in_full_starts_the_file_again() {
        // The server honours no ranges and drops the first two connections
        // at different points; the third attempt restarts and finishes.
        let (url, requests, server) = scripted_server(BODY, |number, _, _| Reply::Body {
            honour_range: false,
            send: match number {
                1 => Some(10),
                2 => Some(20),
                _ => None,
            },
            stall: false,
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        retrying(Duration::from_secs(2), 3)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(starts(&requests), vec![None, Some(10), Some(20)]);
    }

    #[tokio::test]
    async fn busy_and_failing_servers_are_retried() {
        let (url, _, server) = scripted_server(BODY, |number, _, _| match number {
            1 => Reply::Status(503),
            2 => Reply::Status(429),
            3 => Reply::Status(502),
            _ => WHOLE,
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        retrying(Duration::from_secs(2), 4)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
    }

    #[tokio::test]
    async fn retries_go_back_to_the_original_url_for_a_fresh_redirect() {
        // GitHub redirects release assets to short-lived signed URLs. Each
        // one here works once, so a retry that reused it would fail.
        let (url, requests, server) = scripted_server(BODY, |number, path, _| match path {
            "/asset" if number == 1 => Reply::Redirect("/signed-1"),
            "/asset" => Reply::Redirect("/signed-2"),
            "/signed-1" if number == 2 => cut(15),
            "/signed-2" if number == 4 => WHOLE,
            _ => Reply::Status(403),
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        retrying(Duration::from_secs(2), 3)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap();
        server.abort();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                ("/asset".to_owned(), None),
                ("/signed-1".to_owned(), None),
                ("/asset".to_owned(), Some(15)),
                ("/signed-2".to_owned(), Some(15)),
            ]
        );
    }

    #[tokio::test]
    async fn a_download_that_never_progresses_gives_up_with_a_clear_message() {
        let (url, requests, server) = scripted_server(BODY, |_, _, _| Reply::Status(503)).await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        let error = retrying(Duration::from_secs(2), 3)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &path,
                1024,
                None,
            )
            .await
            .unwrap_err();
        server.abort();
        let message = format!("{error:#}");
        assert!(message.contains("3 attempts in a row"), "{message}");
        assert!(message.contains("503"), "{message}");
        assert!(message.contains("re-run"), "{message}");
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_missing_asset_is_not_retried() {
        let (url, requests, server) = scripted_server(BODY, |_, _, _| Reply::Status(404)).await;
        let root = tempfile::tempdir().unwrap();
        let error = retrying(Duration::from_secs(2), 5)
            .fetch(
                &url,
                &crate::upgrade::signing::sha256_hex(BODY),
                &root.path().join("asset"),
                1024,
                None,
            )
            .await
            .unwrap_err();
        server.abort();
        assert!(format!("{error:#}").contains("404"), "{error:#}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metadata_fetches_are_retried_too() {
        const DOCUMENT: &[u8] = br#"{"schema":1,"latest":"v0.1.0","releases":[]}"#;
        let (url, _, server) = scripted_server(DOCUMENT, |number, _, _| match number {
            1 => Reply::Status(503),
            2 => cut(10),
            _ => WHOLE,
        })
        .await;
        let metadata = retrying(Duration::from_secs(2), 3)
            .metadata(&url)
            .await
            .unwrap();
        server.abort();
        assert_eq!(metadata.schema, 1);
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
