//! One way to open an object-store destination from a URL.
//!
//! Log and metrics export, volume-snapshot upload, council backups and the
//! Mayo object-store backend all acknowledge a write and then act on it:
//! pruning the source, flipping an `uploaded` flag, deleting older backups.
//! Every caller goes through [`open`], which fixes two things
//! `object_store::parse_url` gets wrong for us:
//!
//! - it opens `file://` URLs with fsync off, so a power cut could leave an
//!   acknowledged object empty. [`open`] syncs local writes before they
//!   return;
//! - it builds `s3://` and `gs://` stores with `Builder::new()`, which reads
//!   no environment at all, so S3 silently fell back to instance metadata in
//!   `us-east-1` (#695). [`open`] passes each backend its standard variables:
//!   `AWS_*` for S3, `GOOGLE_*` and `SERVICE_ACCOUNT` for GCS.

use object_store::ObjectStore;
use object_store::ObjectStoreScheme;
use object_store::aws::AmazonS3Builder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;

/// Open the store behind `url` and the key prefix inside it, configured
/// from this process's environment.
///
/// `file://` URLs get a local filesystem store that syncs each written file
/// and its directory before the write returns. `s3://` and `gs://` stores
/// take their credentials, region and endpoint from the backend's standard
/// environment variables; their writes are durable once the service
/// acknowledges them.
pub(crate) fn open(url: &url::Url) -> Result<(Box<dyn ObjectStore>, Path), object_store::Error> {
    open_with_env(url, std::env::vars())
}

/// [`open`] with the environment as an argument, so tests can inject
/// credentials without `std::env::set_var` (which is `unsafe` in edition
/// 2024, because another thread may be reading the environment).
pub(crate) fn open_with_env<I>(
    url: &url::Url,
    env: I,
) -> Result<(Box<dyn ObjectStore>, Path), object_store::Error>
where
    I: IntoIterator<Item = (String, String)>,
{
    let (scheme, prefix) = ObjectStoreScheme::parse(url)?;
    let store: Box<dyn ObjectStore> = match scheme {
        ObjectStoreScheme::Local => Box::new(LocalFileSystem::new().with_fsync(true)),
        ObjectStoreScheme::AmazonS3 => Box::new(s3_builder(url, env).build()?),
        ObjectStoreScheme::GoogleCloudStorage => Box::new(gcs_builder(url, env).build()?),
        // Anything else (memory, http, azure) keeps the crate's own
        // handling, which refuses the schemes we don't compile in.
        _ => return object_store::parse_url(url),
    };
    Ok((store, prefix))
}

/// An S3 builder for `url`, configured the way `AmazonS3Builder::from_env`
/// configures one, but from `env` rather than the process environment.
fn s3_builder<I>(url: &url::Url, env: I) -> AmazonS3Builder
where
    I: IntoIterator<Item = (String, String)>,
{
    let builder = env
        .into_iter()
        .filter(|(key, _)| key.starts_with("AWS_"))
        .fold(AmazonS3Builder::new(), |builder, (key, value)| {
            match key.to_ascii_lowercase().parse() {
                Ok(config_key) => builder.with_config(config_key, value),
                Err(_) => builder,
            }
        });
    // The URL names the bucket, and is applied last so it wins.
    builder.with_url(url.as_str())
}

/// A GCS builder for `url`, configured the way
/// `GoogleCloudStorageBuilder::from_env` configures one, but from `env`.
fn gcs_builder<I>(url: &url::Url, env: I) -> GoogleCloudStorageBuilder
where
    I: IntoIterator<Item = (String, String)>,
{
    let builder =
        env.into_iter()
            .fold(GoogleCloudStorageBuilder::new(), |builder, (key, value)| {
                if key == "SERVICE_ACCOUNT" {
                    return builder.with_service_account_path(value);
                }
                if !key.starts_with("GOOGLE_") {
                    return builder;
                }
                match key.to_ascii_lowercase().parse() {
                    Ok(config_key) => builder.with_config(config_key, value),
                    Err(_) => builder,
                }
            });
    builder.with_url(url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::aws::AmazonS3ConfigKey;
    use object_store::gcp::GoogleConfigKey;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn s3_store_uses_region_and_credentials_from_the_environment() {
        let url = url::Url::parse("s3://council-backups/prod").unwrap();
        let variables = env(&[
            ("AWS_REGION", "eu-west-2"),
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("GOOGLE_SERVICE_ACCOUNT", "/not/for/s3.json"),
            ("REGION", "ignored-without-the-prefix"),
        ]);
        let builder = s3_builder(&url, variables.clone());
        let value = |key| builder.get_config_value(&key);
        assert_eq!(
            value(AmazonS3ConfigKey::Region).as_deref(),
            Some("eu-west-2")
        );
        assert_eq!(
            value(AmazonS3ConfigKey::AccessKeyId).as_deref(),
            Some("AKIDEXAMPLE")
        );
        assert_eq!(
            value(AmazonS3ConfigKey::SecretAccessKey).as_deref(),
            Some("secret")
        );

        // Building the store contacts nothing; the region reaches the client.
        let (store, prefix) = open_with_env(&url, variables).unwrap();
        assert!(format!("{store:?}").contains("eu-west-2"), "{store:?}");
        assert_eq!(prefix.as_ref(), "prod");
    }

    #[test]
    fn s3_store_without_a_region_variable_sets_none() {
        let url = url::Url::parse("s3://council-backups/prod").unwrap();
        let builder = s3_builder(&url, env(&[]));
        assert_eq!(builder.get_config_value(&AmazonS3ConfigKey::Region), None);
    }

    #[test]
    fn gcs_store_uses_service_account_from_the_environment() {
        let url = url::Url::parse("gs://council-backups/prod").unwrap();
        let builder = gcs_builder(
            &url,
            env(&[
                ("GOOGLE_SERVICE_ACCOUNT", "/etc/reliaburger/gcs.json"),
                ("AWS_REGION", "eu-west-2"),
            ]),
        );
        assert_eq!(
            builder
                .get_config_value(&GoogleConfigKey::ServiceAccount)
                .as_deref(),
            Some("/etc/reliaburger/gcs.json")
        );

        // The bare `SERVICE_ACCOUNT` the crate's `from_env` also honours.
        let builder = gcs_builder(&url, env(&[("SERVICE_ACCOUNT", "/srv/key.json")]));
        assert_eq!(
            builder
                .get_config_value(&GoogleConfigKey::ServiceAccount)
                .as_deref(),
            Some("/srv/key.json")
        );
    }

    #[test]
    fn file_urls_still_sync_writes() {
        let directory = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(directory.path()).unwrap();
        let (store, prefix) = open_with_env(&url, env(&[("AWS_REGION", "eu-west-2")])).unwrap();
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
