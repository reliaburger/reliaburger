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

/// Parse a byte size like "128Mi", "1Gi", or a bare number of bytes.
///
/// Used for memory, volume sizes and storage caps. Binary suffixes
/// (`Ki`, `Mi`, `Gi`, `Ti`) are powers of 1024; a bare number is bytes,
/// the same as in Kubernetes.
pub fn parse_byte_size(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::InvalidResourceValue {
            value: s.to_string(),
            reason: "empty string".to_string(),
        });
    }

    // Try binary suffixes: Ti, Gi, Mi, Ki
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

    s.parse::<u64>()
        .map_err(|_| ConfigError::InvalidResourceValue {
            value: s.to_string(),
            reason: "expected a number of bytes with optional suffix (Ki, Mi, Gi, Ti)".to_string(),
        })
}

/// Parse a CPU quantity into millicores.
///
/// Follows the Kubernetes convention: a bare number is whole cores
/// (`"2"` is 2000 millicores, `"0.5"` is 500), and the `m` suffix is
/// millicores (`"250m"`). Anything finer than one millicore is rejected
/// rather than silently rounded.
pub fn parse_cpu_millicores(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    let invalid = |reason: &str| ConfigError::InvalidResourceValue {
        value: s.to_string(),
        reason: reason.to_string(),
    };
    if s.is_empty() {
        return Err(invalid("empty string"));
    }
    if let Some(num) = s.strip_suffix('m') {
        return parse_num(num, 1, s);
    }

    // Cores, possibly with a fractional part. Parsed by hand rather than
    // through f64 so "0.1" is exactly 100 millicores, not 99.999...
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction) {
        return Err(invalid(
            "expected cores (\"2\", \"0.5\") or millicores (\"500m\")",
        ));
    }
    if fraction.len() > 3 {
        return Err(invalid("finer than one millicore"));
    }
    let whole_millicores = if whole.is_empty() {
        0
    } else {
        parse_num(whole, 1000, s)?
    };
    let fraction_millicores = if fraction.is_empty() {
        0
    } else {
        // Right-pad to three digits: ".5" is 500 millicores, ".05" is 50.
        format!("{fraction:0<3}")
            .parse::<u64>()
            .map_err(|_| invalid("invalid fractional cores"))?
    };
    whole_millicores
        .checked_add(fraction_millicores)
        .ok_or_else(|| invalid("value overflows 64-bit millicore count"))
}

fn parse_num(num_str: &str, multiplier: u64, original: &str) -> Result<u64, ConfigError> {
    let n: u64 = num_str
        .parse()
        .map_err(|_| ConfigError::InvalidResourceValue {
            value: original.to_string(),
            reason: format!("{num_str:?} is not a valid number"),
        })?;
    // A huge memory string like "99999999999999999999Gi" must be rejected
    // as invalid, not silently wrapped by an overflowing `n * multiplier`
    // into some small nonsense value (DEP9).
    n.checked_mul(multiplier)
        .ok_or_else(|| ConfigError::InvalidResourceValue {
            value: original.to_string(),
            reason: "value overflows 64-bit count".to_string(),
        })
}

// ---------------------------------------------------------------------------
// ResourceRange
// ---------------------------------------------------------------------------

/// A request-limit pair for CPU or memory resources.
///
/// Parsed from strings like `"128Mi-512Mi"` (request 128Mi, limit 512Mi)
/// or `"256Mi"` (request and limit are equal). CPU values are stored in
/// millicores, memory in bytes. The same struct serves both, so the unit
/// lives in the parser: config fields pick [`cpu_range`] or
/// [`memory_range`] with `#[serde(with = ...)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRange {
    pub request: u64,
    pub limit: u64,
}

impl ResourceRange {
    /// Parse a CPU range such as `"0.5-2"` or `"100m-500m"` into millicores.
    pub fn parse_cpu(s: &str) -> Result<Self, ConfigError> {
        Self::parse_with(s, parse_cpu_millicores)
    }

    /// Parse a memory range such as `"128Mi-512Mi"` into bytes.
    pub fn parse_memory(s: &str) -> Result<Self, ConfigError> {
        Self::parse_with(s, parse_byte_size)
    }

    fn parse_with(
        s: &str,
        parse_value: fn(&str) -> Result<u64, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let Some((req_str, lim_str)) = s.split_once('-') else {
            let value = parse_value(s)?;
            return Ok(Self {
                request: value,
                limit: value,
            });
        };
        let request = parse_value(req_str)?;
        let limit = parse_value(lim_str)?;
        if request > limit {
            return Err(ConfigError::InvalidResourceRange {
                value: s.to_string(),
                reason: format!("request ({req_str}) exceeds limit ({lim_str})"),
            });
        }
        Ok(Self { request, limit })
    }

    /// Render a CPU range as millicores, e.g. `"100m-500m"` or `"2000m"`.
    pub fn to_cpu_string(&self) -> String {
        self.render("m")
    }

    /// Render a memory range as bytes, e.g. `"134217728-536870912"`.
    pub fn to_memory_string(&self) -> String {
        self.render("")
    }

    /// Render the range with a unit suffix, collapsing equal halves.
    fn render(&self, suffix: &str) -> String {
        if self.request == self.limit {
            format!("{}{suffix}", self.request)
        } else {
            format!("{}{suffix}-{}{suffix}", self.request, self.limit)
        }
    }
}

/// Serde adapter for an optional CPU range: bare numbers are cores.
///
/// Serialises as millicores with the `m` suffix, so a value always
/// round-trips (writing a bare `"500"` back would read as 500 cores).
pub mod cpu_range {
    use super::ResourceRange;
    use serde::{Deserialize, Deserializer, Serializer, de};

    /// Write the range as millicores, e.g. `"100m-500m"`.
    pub fn serialize<S: Serializer>(
        value: &Option<ResourceRange>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(range) => serializer.serialize_str(&range.to_cpu_string()),
            None => serializer.serialize_none(),
        }
    }

    /// Read a CPU range string; see [`ResourceRange::parse_cpu`].
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<ResourceRange>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|s| ResourceRange::parse_cpu(&s).map_err(de::Error::custom))
            .transpose()
    }
}

/// Serde adapter for an optional memory range: bare numbers are bytes.
pub mod memory_range {
    use super::ResourceRange;
    use serde::{Deserialize, Deserializer, Serializer, de};

    /// Write the range as bytes, e.g. `"134217728-536870912"`.
    pub fn serialize<S: Serializer>(
        value: &Option<ResourceRange>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(range) => serializer.serialize_str(&range.to_memory_string()),
            None => serializer.serialize_none(),
        }
    }

    /// Read a memory range string; see [`ResourceRange::parse_memory`].
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<ResourceRange>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|s| ResourceRange::parse_memory(&s).map_err(de::Error::custom))
            .transpose()
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    // -- parse_byte_size ------------------------------------------------------

    #[test]
    fn parse_byte_size_ki() {
        assert_eq!(parse_byte_size("1Ki").unwrap(), 1024);
    }

    #[test]
    fn parse_byte_size_mi() {
        assert_eq!(parse_byte_size("1Mi").unwrap(), 1_048_576);
    }

    #[test]
    fn parse_byte_size_gi() {
        assert_eq!(parse_byte_size("1Gi").unwrap(), 1_073_741_824);
    }

    #[test]
    fn parse_byte_size_ti() {
        assert_eq!(parse_byte_size("1Ti").unwrap(), 1_099_511_627_776);
    }

    #[test]
    fn parse_byte_size_bare_number_is_bytes() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_byte_size_rejects_millicore_suffix() {
        // `m` is a CPU unit; "500m" of memory is a mistake, not 500 bytes.
        assert!(parse_byte_size("500m").is_err());
    }

    #[test]
    fn parse_byte_size_empty_string_rejected() {
        assert!(parse_byte_size("").is_err());
    }

    #[test]
    fn parse_byte_size_invalid_suffix_rejected() {
        assert!(parse_byte_size("100X").is_err());
    }

    #[test]
    fn parse_byte_size_overflow_rejected() {
        // A number that overflows u64 once multiplied by the suffix must be
        // a validation error, not a wrapped small value (DEP9).
        let err = parse_byte_size("99999999999999999999Gi");
        assert!(matches!(err, Err(ConfigError::InvalidResourceValue { .. })));
        // Also the bare-parse overflow (number itself too large for u64).
        assert!(parse_byte_size("99999999999999999999999").is_err());
    }

    // -- parse_cpu_millicores -------------------------------------------------

    #[test]
    fn parse_cpu_bare_integer_is_cores() {
        assert_eq!(parse_cpu_millicores("2").unwrap(), 2000);
    }

    #[test]
    fn parse_cpu_decimal_cores() {
        assert_eq!(parse_cpu_millicores("0.5").unwrap(), 500);
        assert_eq!(parse_cpu_millicores(".25").unwrap(), 250);
        assert_eq!(parse_cpu_millicores("1.5").unwrap(), 1500);
        assert_eq!(parse_cpu_millicores("0.001").unwrap(), 1);
        assert_eq!(parse_cpu_millicores("2.").unwrap(), 2000);
    }

    #[test]
    fn parse_cpu_millicore_suffix() {
        assert_eq!(parse_cpu_millicores("500m").unwrap(), 500);
    }

    #[test]
    fn parse_cpu_rejects_sub_millicore_precision() {
        assert!(parse_cpu_millicores("0.0005").is_err());
    }

    #[test]
    fn parse_cpu_rejects_garbage() {
        for bad in ["", ".", "-1", "1Gi", "abc", "1.2.3", "0.5m", "1e3"] {
            assert!(parse_cpu_millicores(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn parse_cpu_overflow_rejected() {
        assert!(parse_cpu_millicores("99999999999999999999").is_err());
        assert!(parse_cpu_millicores("18446744073709551615").is_err());
    }

    // -- ResourceRange --------------------------------------------------------

    #[test]
    fn parse_resource_range_cpu_with_range() {
        let rr = ResourceRange::parse_cpu("100m-500m").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 100,
                limit: 500
            }
        );
    }

    #[test]
    fn parse_resource_range_cpu_cores_range() {
        let rr = ResourceRange::parse_cpu("0.5-2").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 500,
                limit: 2000
            }
        );
    }

    #[test]
    fn parse_resource_range_memory_with_range() {
        let rr = ResourceRange::parse_memory("128Mi-512Mi").unwrap();
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
        let rr = ResourceRange::parse_memory("256Mi").unwrap();
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
    fn parse_resource_range_bare_cpu_number_is_cores() {
        let rr = ResourceRange::parse_cpu("2").unwrap();
        assert_eq!(
            rr,
            ResourceRange {
                request: 2000,
                limit: 2000
            }
        );
    }

    #[test]
    fn parse_resource_range_bare_memory_number_is_bytes() {
        let rr = ResourceRange::parse_memory("1000").unwrap();
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
        assert!(ResourceRange::parse_memory("100X-200X").is_err());
        assert!(ResourceRange::parse_cpu("100X-200X").is_err());
    }

    #[test]
    fn parse_resource_range_request_exceeds_limit_rejected() {
        assert!(ResourceRange::parse_cpu("500m-100m").is_err());
        assert!(ResourceRange::parse_cpu("2-1").is_err());
    }

    #[test]
    fn parse_resource_range_empty_string_rejected() {
        assert!(ResourceRange::parse_cpu("").is_err());
        assert!(ResourceRange::parse_memory("").is_err());
    }

    // -- ResourceRange serde round-trip ---------------------------------------

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Resources {
        #[serde(default, with = "cpu_range", skip_serializing_if = "Option::is_none")]
        cpu: Option<ResourceRange>,
        #[serde(
            default,
            with = "memory_range",
            skip_serializing_if = "Option::is_none"
        )]
        memory: Option<ResourceRange>,
    }

    #[test]
    fn resource_range_deserialise_from_toml() {
        let r: Resources = toml::from_str(
            r#"
            cpu = "100m-500m"
            memory = "128Mi"
            "#,
        )
        .unwrap();
        assert_eq!(
            r.cpu,
            Some(ResourceRange {
                request: 100,
                limit: 500
            })
        );
        assert_eq!(r.memory.map(|m| m.limit), Some(128 * 1024 * 1024));
    }

    #[test]
    fn resource_range_missing_fields_are_none() {
        let r: Resources = toml::from_str("").unwrap();
        assert_eq!(
            r,
            Resources {
                cpu: None,
                memory: None
            }
        );
    }

    #[test]
    fn cpu_range_serialises_with_millicore_suffix_and_round_trips() {
        // A bare "500" would read back as 500 cores, so the adapter must
        // always write the `m` suffix.
        let original = Resources {
            cpu: Some(ResourceRange {
                request: 500,
                limit: 2000,
            }),
            memory: Some(ResourceRange {
                request: 1024,
                limit: 1024,
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#"{"cpu":"500m-2000m","memory":"1024"}"#);
        let decoded: Resources = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
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
