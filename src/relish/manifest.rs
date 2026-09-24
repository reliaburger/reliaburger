//! What `relish apply` reads: a Reliaburger TOML file, or Kubernetes YAML,
//! from a local path or an `https://` URL.
//!
//! Kubernetes YAML goes through the importer in memory, so "run your
//! Kubernetes app" is one command. The migration report still goes to
//! stderr: applying a manifest mustn't hide what the import approximated or
//! dropped. `relish import` stays for people who want the TOML.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;

use super::RelishError;

/// Largest manifest we'll download. Real manifests are kilobytes.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// How long a manifest download may take, connection included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a manifest comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    /// A local file.
    File(PathBuf),
    /// An `https://` URL.
    Url(String),
}

impl ManifestSource {
    /// Read a command-line argument: `https://…` is a URL, anything else a
    /// path. Plain `http://` is refused: a manifest decides what runs.
    pub fn parse(argument: &str) -> Result<Self, RelishError> {
        if argument.starts_with("https://") {
            return Ok(Self::Url(argument.to_string()));
        }
        if argument.starts_with("http://") {
            return Err(RelishError::ManifestFetch {
                url: argument.to_string(),
                reason: "only https:// URLs are accepted".to_string(),
            });
        }
        Ok(Self::File(PathBuf::from(argument)))
    }
}

/// The manifest languages `relish apply` understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestFormat {
    /// Reliaburger's own TOML.
    Reliaburger,
    /// Kubernetes YAML (any document with top-level `apiVersion` and `kind`).
    Kubernetes,
}

/// Tell Kubernetes YAML from Reliaburger TOML by its shape.
///
/// A top-level `apiVersion:` line is invalid TOML, so a document with one
/// (and a `kind:`) can only be Kubernetes.
pub fn detect_format(content: &str) -> ManifestFormat {
    let top_level = |key: &str| content.lines().any(|line| line.starts_with(key));
    if top_level("apiVersion:") && top_level("kind:") {
        ManifestFormat::Kubernetes
    } else {
        ManifestFormat::Reliaburger
    }
}

/// A manifest, parsed into Reliaburger config.
#[derive(Debug)]
pub struct LoadedManifest {
    /// The config to apply.
    pub config: Config,
    /// The import's migration report, when the manifest was Kubernetes YAML.
    pub migration_report: Option<String>,
}

/// Read and parse a manifest from `source`.
pub async fn load(source: &ManifestSource) -> Result<LoadedManifest, RelishError> {
    let content = match source {
        ManifestSource::File(path) => read_file(path)?,
        ManifestSource::Url(url) => fetch(url).await?,
    };
    parse(&content)
}

/// Parse manifest text, importing it first when it's Kubernetes YAML.
pub fn parse(content: &str) -> Result<LoadedManifest, RelishError> {
    match detect_format(content) {
        ManifestFormat::Reliaburger => Ok(LoadedManifest {
            config: Config::parse(content)?,
            migration_report: None,
        }),
        ManifestFormat::Kubernetes => import(content),
    }
}

#[cfg(feature = "kubernetes")]
fn import(content: &str) -> Result<LoadedManifest, RelishError> {
    let result = super::k8s_import::import_from_yaml(content)?;
    Ok(LoadedManifest {
        config: result.config,
        migration_report: Some(result.report.to_string()),
    })
}

#[cfg(not(feature = "kubernetes"))]
fn import(_content: &str) -> Result<LoadedManifest, RelishError> {
    Err(RelishError::FormatFailed(
        "this relish was built without Kubernetes support; convert the manifest to TOML"
            .to_string(),
    ))
}

fn read_file(path: &Path) -> Result<String, RelishError> {
    std::fs::read_to_string(path).map_err(|source| {
        crate::config::ConfigError::ReadFile {
            path: path.to_path_buf(),
            source,
        }
        .into()
    })
}

/// Download a manifest over HTTPS, bounded in size and time. Redirects must
/// stay on HTTPS too.
async fn fetch(url: &str) -> Result<String, RelishError> {
    let failed = |reason: String| RelishError::ManifestFetch {
        url: url.to_string(),
        reason,
    };
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| failed(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(format!("server answered {}", response.status())));
    }
    read_bounded(response, MAX_MANIFEST_BYTES)
        .await
        .map_err(failed)
}

/// Read a response body as UTF-8, refusing more than `limit` bytes.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("manifest is larger than {limit} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > limit {
            return Err(format!("manifest is larger than {limit} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "manifest is not UTF-8 text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPLOYMENT: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: web
        image: nginx:1
        ports:
        - containerPort: 80
"#;

    #[test]
    fn https_urls_and_paths_parse_and_plain_http_is_refused() {
        assert_eq!(
            ManifestSource::parse("https://example.com/app.yaml").unwrap(),
            ManifestSource::Url("https://example.com/app.yaml".into())
        );
        assert_eq!(
            ManifestSource::parse("deploy/app.yaml").unwrap(),
            ManifestSource::File("deploy/app.yaml".into())
        );
        assert!(matches!(
            ManifestSource::parse("http://example.com/app.yaml"),
            Err(RelishError::ManifestFetch { .. })
        ));
    }

    #[test]
    fn kubernetes_yaml_is_told_apart_from_toml() {
        assert_eq!(detect_format(DEPLOYMENT), ManifestFormat::Kubernetes);
        assert_eq!(
            detect_format("[app.web]\nimage = \"nginx:1\"\n"),
            ManifestFormat::Reliaburger
        );
        // A TOML string that happens to mention apiVersion isn't YAML.
        assert_eq!(
            detect_format("[app.web]\nimage = \"x\"\n# apiVersion: v1\n"),
            ManifestFormat::Reliaburger
        );
    }

    #[cfg(feature = "kubernetes")]
    #[test]
    fn kubernetes_yaml_imports_in_memory_with_its_report() {
        let loaded = parse(DEPLOYMENT).unwrap();
        let app = &loaded.config.app["web"];
        assert_eq!(app.image.as_deref(), Some("nginx:1"));
        assert_eq!(app.port, Some(80));
        let report = loaded.migration_report.unwrap();
        assert!(report.contains("Deployment/web"), "{report}");
    }

    #[test]
    fn toml_parses_without_a_report() {
        let loaded = parse("[app.web]\nimage = \"nginx:1\"\n").unwrap();
        assert!(loaded.config.app.contains_key("web"));
        assert!(loaded.migration_report.is_none());
    }

    fn response(body: &'static str) -> reqwest::Response {
        reqwest::Response::from(axum::http::Response::new(body))
    }

    #[tokio::test]
    async fn downloads_are_bounded_in_size() {
        assert_eq!(
            read_bounded(response("apiVersion: v1"), 64).await.unwrap(),
            "apiVersion: v1"
        );
        let refused = read_bounded(response("0123456789"), 4).await;
        assert!(refused.unwrap_err().contains("larger than 4 bytes"));
    }

    #[tokio::test]
    async fn a_missing_file_is_a_read_error() {
        let result = load(&ManifestSource::File("/definitely/missing.yaml".into())).await;
        assert!(matches!(result, Err(RelishError::Config(_))), "{result:?}");
    }
}
