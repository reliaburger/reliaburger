//! One way to open an object-store destination from a URL.
//!
//! Log and metrics export, volume-snapshot upload, council backups and the
//! Mayo object-store backend all acknowledge a write and then act on it:
//! pruning the source, flipping an `uploaded` flag, deleting older backups.
//! `object_store::parse_url` opens `file://` URLs with fsync off, so a power
//! cut could leave an acknowledged object empty. Every caller goes through
//! [`open`] instead, which syncs local writes before they return.

use object_store::ObjectStore;
use object_store::local::LocalFileSystem;
use object_store::path::Path;

/// Open the store behind `url` and the key prefix inside it.
///
/// `file://` URLs get a local filesystem store that syncs each written file
/// and its directory before the write returns. Other schemes (`s3://`,
/// `gs://`) are durable once the service acknowledges them and pass through
/// unchanged.
pub(crate) fn open(url: &url::Url) -> Result<(Box<dyn ObjectStore>, Path), object_store::Error> {
    let (store, prefix) = object_store::parse_url(url)?;
    if url.scheme() == "file" {
        return Ok((Box::new(LocalFileSystem::new().with_fsync(true)), prefix));
    }
    Ok((store, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_sync_writes() {
        let directory = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(directory.path()).unwrap();
        let (store, prefix) = open(&url).unwrap();
        assert!(format!("{store:?}").contains("fsync: true"), "{store:?}");
        assert_eq!(
            format!("/{prefix}/"),
            directory.path().to_str().unwrap().to_string() + "/"
        );
    }

    #[tokio::test]
    async fn synced_local_store_still_round_trips_objects() {
        use object_store::ObjectStoreExt;
        let directory = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(directory.path()).unwrap();
        let (store, prefix) = open(&url).unwrap();
        let key = prefix.join("object");
        store
            .put(&key, object_store::PutPayload::from_static(b"bytes"))
            .await
            .unwrap();
        let bytes = store.get(&key).await.unwrap().bytes().await.unwrap();
        assert_eq!(&bytes[..], b"bytes");
    }

    #[test]
    fn unsupported_schemes_are_refused() {
        let url = url::Url::parse("ftp://example.invalid/backups").unwrap();
        assert!(open(&url).is_err());
    }
}
