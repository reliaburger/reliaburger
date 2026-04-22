//! Safe UI data types for the Brioche web dashboard.
//!
//! These types ensure sensitive data (encrypted env values) never
//! reaches the browser. They also carry chart configuration for
//! client-side uPlot initialisation.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::bun::agent::InstanceStatus;
use crate::config::EnvValue;
use crate::meat::deploy_types::DeployHistoryEntry;

// ---------------------------------------------------------------------------
// SafeEnvValue
// ---------------------------------------------------------------------------

/// An environment variable value safe for UI display.
///
/// Encrypted values are replaced with `"[encrypted]"` so the raw
/// ciphertext never leaves the server.
#[derive(Debug, Clone, Serialize)]
pub struct SafeEnvValue {
    pub key: String,
    pub value: String,
    pub encrypted: bool,
}

/// Convert an app's env map into a safe representation.
pub fn safe_env(env: &BTreeMap<String, EnvValue>) -> Vec<SafeEnvValue> {
    env.iter()
        .map(|(k, v)| SafeEnvValue {
            key: k.clone(),
            value: match v {
                EnvValue::Plain(s) => s.clone(),
                EnvValue::Encrypted(_) => "[encrypted]".to_string(),
            },
            encrypted: v.is_encrypted(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ChartConfig
// ---------------------------------------------------------------------------

/// Configuration for a single uPlot time-series chart.
///
/// Serialised as JSON into an HTML `data-chart-config` attribute.
/// The client JS reads this to create uPlot instances and start
/// periodic data fetching.
#[derive(Debug, Clone, Serialize)]
pub struct ChartConfig {
    pub endpoint: String,
    pub title: String,
    pub y_label: String,
    pub refresh_secs: u32,
    pub range_secs: u64,
}

// ---------------------------------------------------------------------------
// Page data structs
// ---------------------------------------------------------------------------

/// Data backing the app detail page.
pub struct AppDetailData {
    pub app_name: String,
    pub namespace: String,
    pub state: String,
    pub instances: Vec<InstanceStatus>,
    pub env: Vec<SafeEnvValue>,
    pub deploy_history: Vec<DeployHistoryEntry>,
    pub charts: Vec<ChartConfig>,
}

/// Data backing the node detail page.
pub struct NodeDetailData {
    pub name: String,
    pub state: String,
    pub app_count: usize,
    pub apps: Vec<InstanceStatus>,
    pub charts: Vec<ChartConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_env_masks_encrypted_values() {
        let mut env = BTreeMap::new();
        env.insert("PLAIN".to_string(), EnvValue::Plain("hello".to_string()));
        env.insert(
            "SECRET".to_string(),
            EnvValue::Encrypted("ENC[AGE:abc123]".to_string()),
        );

        let safe = safe_env(&env);
        assert_eq!(safe.len(), 2);

        let plain = safe.iter().find(|e| e.key == "PLAIN").unwrap();
        assert_eq!(plain.value, "hello");
        assert!(!plain.encrypted);

        let secret = safe.iter().find(|e| e.key == "SECRET").unwrap();
        assert_eq!(secret.value, "[encrypted]");
        assert!(secret.encrypted);
    }

    #[test]
    fn safe_env_never_contains_enc_prefix() {
        let mut env = BTreeMap::new();
        env.insert(
            "DB_URL".to_string(),
            EnvValue::Encrypted("ENC[AGE:longciphertext==]".to_string()),
        );
        env.insert(
            "LOG_LEVEL".to_string(),
            EnvValue::Plain("debug".to_string()),
        );

        let safe = safe_env(&env);
        let json = serde_json::to_string(&safe).unwrap();
        assert!(!json.contains("ENC[AGE:"));
    }

    #[test]
    fn chart_config_serialises() {
        let cfg = ChartConfig {
            endpoint: "/v1/metrics?name=cpu".to_string(),
            title: "CPU Usage".to_string(),
            y_label: "%".to_string(),
            refresh_secs: 10,
            range_secs: 3600,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"endpoint\":\"/v1/metrics?name=cpu\""));
        assert!(json.contains("\"refresh_secs\":10"));
    }
}
