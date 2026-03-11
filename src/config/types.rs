/// Shared types used across config structs.
///
/// `Replicas`, `ResourceRange`, `EnvValue`, `ConfigFileSpec`, and
/// `VolumeSpec` appear in both App and Job specs and are defined
/// here to avoid circular dependencies between modules.
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize, de};

use super::error::ConfigError;

// ---------------------------------------------------------------------------
// Resource value parsing
// ---------------------------------------------------------------------------

/// Parse a resource string like "128Mi", "500m", "1Gi", or a bare number.
///
/// Returns the value in base units:
/// - Memory suffixes (Ki, Mi, Gi, Ti) return bytes
/// - CPU suffix (m) returns millicores
/// - Bare numbers return the raw integer
pub fn parse_resource_value(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::InvalidResourceValue {
            value: s.to_string(),
            reason: "empty string".to_string(),
        });
    }

    // Try binary suffixes (memory): Ti, Gi, Mi, Ki
    // Order matters — check longer suffixes first
    if let Some(num) = s.strip_suffix("Ti") {
        return parse_num(num, 1024 * 1024 * 1024 * 1024, s);
    }
    if let Some(num) = s.strip_suffix("Gi") {
        return parse_num(num, 1024 * 1024 * 1024, s);
    }
    if let Some(num) = s.strip_suffix("Mi") {
        return parse_num(num, 1024 * 1024, s);
    }
    if let Some(num) = s.strip_suffix("Ki") {
        return parse_num(num, 1024, s);
    }

    // CPU suffix: millicores
    if let Some(num) = s.strip_suffix('m') {
        return parse_num(num, 1, s);
    }

    // Bare number
    s.parse::<u64>()
        .map_err(|_| ConfigError::InvalidResourceValue {
            value: s.to_string(),
            reason: "expected a number with optional suffix (Ki, Mi, Gi, Ti, m)".to_string(),
        })
}

fn parse_num(num_str: &str, multiplier: u64, original: &str) -> Result<u64, ConfigError> {
    let n: u64 = num_str
        .parse()
        .map_err(|_| ConfigError::InvalidResourceValue {
            value: original.to_string(),
            reason: format!("{num_str:?} is not a valid number"),
        })?;
    Ok(n * multiplier)
}

// ---------------------------------------------------------------------------
// ResourceRange
// ---------------------------------------------------------------------------

/// A request-limit pair for CPU or memory resources.
///
/// Parsed from strings like `"128Mi-512Mi"` (request 128Mi, limit 512Mi)
/// or `"256Mi"` (request and limit are equal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRange {
    pub request: u64,
    pub limit: u64,
}

impl ResourceRange {
    /// Parse a resource range from a string.
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        if let Some((req_str, lim_str)) = s.split_once('-') {
            let request = parse_resource_value(req_str)?;
            let limit = parse_resource_value(lim_str)?;
            if request > limit {
                return Err(ConfigError::InvalidResourceRange {
                    value: s.to_string(),
                    reason: format!("request ({req_str}) exceeds limit ({lim_str})"),
                });
            }
            Ok(Self { request, limit })
        } else {
            let value = parse_resource_value(s)?;
            Ok(Self {
                request: value,
                limit: value,
            })
        }
    }
}

impl fmt::Display for ResourceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.request == self.limit {
            write!(f, "{}", self.request)
        } else {
            write!(f, "{}-{}", self.request, self.limit)
        }
    }
}

impl Serialize for ResourceRange {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize back to the original string format
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ResourceRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ResourceRange::parse(&s).map_err(de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Replicas
// ---------------------------------------------------------------------------

/// Replica count for an App.
///
/// Either a fixed integer (`replicas = 3`) or daemon mode
/// (`replicas = "*"`, one instance per node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replicas {
    /// Run exactly this many replicas across the cluster.
    Fixed(u32),
    /// Run one replica on every node (daemon mode).
    DaemonSet,
}

impl Default for Replicas {
    fn default() -> Self {
        Replicas::Fixed(1)
    }
}

impl fmt::Display for Replicas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Replicas::Fixed(n) => write!(f, "{n}"),
            Replicas::DaemonSet => write!(f, "*"),
        }
    }
}

impl Serialize for Replicas {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Replicas::Fixed(n) => serializer.serialize_u32(*n),
            Replicas::DaemonSet => serializer.serialize_str("*"),
        }
    }
}

impl<'de> Deserialize<'de> for Replicas {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ReplicasVisitor;

        impl<'de> de::Visitor<'de> for ReplicasVisitor {
            type Value = Replicas;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a positive integer or \"*\"")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v <= 0 {
                    return Err(E::custom("replicas must be a positive integer"));
                }
                u32::try_from(v)
                    .map(Replicas::Fixed)
                    .map_err(|_| E::custom("replicas value too large"))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    return Err(E::custom("replicas must be a positive integer"));
                }
                u32::try_from(v)
                    .map(Replicas::Fixed)
                    .map_err(|_| E::custom("replicas value too large"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "*" {
                    Ok(Replicas::DaemonSet)
                } else {
                    Err(E::custom(
                        "invalid replicas value: expected a positive integer or \"*\"",
                    ))
                }
            }
        }

        deserializer.deserialize_any(ReplicasVisitor)
    }
}

// ---------------------------------------------------------------------------
// EnvValue
// ---------------------------------------------------------------------------

/// An environment variable value — either plain text or an encrypted secret.
///
/// Values starting with `ENC[AGE:` are treated as age-encrypted secrets
/// and decrypted at injection time. Everything else is plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Plain(String),
    Encrypted(String),
}

impl EnvValue {
    const ENCRYPTED_PREFIX: &str = "ENC[AGE:";

    /// Returns `true` if this value is encrypted.
    pub fn is_encrypted(&self) -> bool {
        matches!(self, EnvValue::Encrypted(_))
    }

    /// Returns the raw string value (with prefix for encrypted values).
    pub fn as_str(&self) -> &str {
        match self {
            EnvValue::Plain(s) | EnvValue::Encrypted(s) => s,
        }
    }
}

impl Serialize for EnvValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.starts_with(Self::ENCRYPTED_PREFIX) {
            Ok(EnvValue::Encrypted(s))
        } else {
            Ok(EnvValue::Plain(s))
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigFileSpec
// ---------------------------------------------------------------------------

/// A configuration file injected into a workload's filesystem.
///
/// Exactly one of `content` (inline) or `source` (git path) must be set.
/// Validated in the validation pass, not at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFileSpec {
    /// Absolute path inside the container where the file is mounted.
    pub path: PathBuf,
    /// Inline file content.
    pub content: Option<String>,
    /// Relative path in the git repository.
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// VolumeSpec
// ---------------------------------------------------------------------------

/// Local persistent storage attached to an App.
///
/// Volumes survive container restarts but are tied to the physical node.
/// Two modes:
///
/// - **HostPath:** set `source` to an absolute path on the host. The
///   directory is bind-mounted directly (like Kubernetes `hostPath`).
/// - **Managed:** omit `source`. Reliaburger creates a directory under
///   `storage.volumes/{namespace}/{app}` and bind-mounts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Mount path inside the container.
    pub path: PathBuf,
    /// Host path to bind mount from. If omitted, Reliaburger manages
    /// the storage directory automatically.
    pub source: Option<PathBuf>,
    /// Size limit, e.g. "10Gi". Optional — enforced in Phase 5.
    pub size: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_resource_value -------------------------------------------------

    #[test]
    fn parse_resource_value_ki() {
        assert_eq!(parse_resource_value("1Ki").unwrap(), 1024);
    }

    #[test]
    fn parse_resource_value_mi() {
        assert_eq!(parse_resource_value("1Mi").unwrap(), 1_048_576);
    }

    #[test]
    fn parse_resource_value_gi() {
        assert_eq!(parse_resource_value("1Gi").unwrap(), 1_073_741_824);
    }

    #[test]
    fn parse_resource_value_ti() {
        assert_eq!(parse_resource_value("1Ti").unwrap(), 1_099_511_627_776);
    }

    #[test]
    fn parse_resource_value_millicores() {
        assert_eq!(parse_resource_value("500m").unwrap(), 500);
    }

    #[test]
    fn parse_resource_value_bare_number() {
        assert_eq!(parse_resource_value("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_resource_value_empty_string_rejected() {
        assert!(parse_resource_value("").is_err());
    }

    #[test]
    fn parse_resource_value_invalid_suffix_rejected() {
        assert!(parse_resource_value("100X").is_err());
    }

    // -- ResourceRange --------------------------------------------------------

    #[test]
    fn parse_resource_range_cpu_with_range() {
        let rr = ResourceRange::parse("100m-500m").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 100,
                limit: 500
            }
        );
    }

    #[test]
    fn parse_resource_range_memory_with_range() {
        let rr = ResourceRange::parse("128Mi-512Mi").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 128 * 1024 * 1024,
                limit: 512 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn parse_resource_range_single_value() {
        let rr = ResourceRange::parse("256Mi").unwrap();
        let expected = 256 * 1024 * 1024;
        assert_eq!(
            rr,
            ResourceRange {
                request: expected,
                limit: expected
            }
        );
    }

    #[test]
    fn parse_resource_range_bare_number() {
        let rr = ResourceRange::parse("1000").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 1000,
                limit: 1000
            }
        );
    }

    #[test]
    fn parse_resource_range_invalid_suffix_rejected() {
        assert!(ResourceRange::parse("100X-200X").is_err());
    }

    #[test]
    fn parse_resource_range_request_exceeds_limit_rejected() {
        assert!(ResourceRange::parse("500m-100m").is_err());
    }

    #[test]
    fn parse_resource_range_empty_string_rejected() {
        assert!(ResourceRange::parse("").is_err());
    }

    // -- ResourceRange serde round-trip ---------------------------------------

    #[test]
    fn resource_range_deserialise_from_toml() {
        #[derive(Deserialize)]
        struct Wrapper {
            cpu: ResourceRange,
        }
        let w: Wrapper = toml::from_str(r#"cpu = "100m-500m""#).unwrap();
        assert_eq!(
            w.cpu,
            ResourceRange {
                request: 100,
                limit: 500
            }
        );
    }

    // -- Replicas -------------------------------------------------------------

    #[test]
    fn replicas_deserialise_integer() {
        #[derive(Deserialize)]
        struct W {
            replicas: Replicas,
        }
        let w: W = toml::from_str("replicas = 3").unwrap();
        assert_eq!(w.replicas, Replicas::Fixed(3));
    }

    #[test]
    fn replicas_deserialise_star() {
        #[derive(Deserialize)]
        struct W {
            replicas: Replicas,
        }
        let w: W = toml::from_str(r#"replicas = "*""#).unwrap();
        assert_eq!(w.replicas, Replicas::DaemonSet);
    }

    #[test]
    fn replicas_deserialise_zero_rejected() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct W {
            replicas: Replicas,
        }
        assert!(toml::from_str::<W>("replicas = 0").is_err());
    }

    #[test]
    fn replicas_deserialise_invalid_string_rejected() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct W {
            replicas: Replicas,
        }
        assert!(toml::from_str::<W>(r#"replicas = "all""#).is_err());
    }

    #[test]
    fn replicas_default_is_one() {
        assert_eq!(Replicas::default(), Replicas::Fixed(1));
    }

    #[test]
    fn replicas_round_trip_fixed() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            replicas: Replicas,
        }
        let original = W {
            replicas: Replicas::Fixed(5),
        };
        let toml_str = toml::to_string(&original).unwrap();
        let decoded: W = toml::from_str(&toml_str).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn replicas_round_trip_daemon() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            replicas: Replicas,
        }
        let original = W {
            replicas: Replicas::DaemonSet,
        };
        let toml_str = toml::to_string(&original).unwrap();
        let decoded: W = toml::from_str(&toml_str).unwrap();
        assert_eq!(original, decoded);
    }

    // -- EnvValue -------------------------------------------------------------

    #[test]
    fn env_value_plain() {
        #[derive(Deserialize)]
        struct W {
            val: EnvValue,
        }
        let w: W = toml::from_str(r#"val = "hello""#).unwrap();
        assert_eq!(w.val, EnvValue::Plain("hello".to_string()));
        assert!(!w.val.is_encrypted());
    }

    #[test]
    fn env_value_encrypted() {
        #[derive(Deserialize)]
        struct W {
            val: EnvValue,
        }
        let w: W = toml::from_str(r#"val = "ENC[AGE:abc123]""#).unwrap();
        assert_eq!(w.val, EnvValue::Encrypted("ENC[AGE:abc123]".to_string()));
        assert!(w.val.is_encrypted());
    }

    #[test]
    fn env_value_empty_string() {
        #[derive(Deserialize)]
        struct W {
            val: EnvValue,
        }
        let w: W = toml::from_str(r#"val = """#).unwrap();
        assert_eq!(w.val, EnvValue::Plain(String::new()));
    }
}
