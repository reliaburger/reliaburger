/// Gossip protocol configuration.
///
/// Timing parameters for the SWIM probe cycle. Defaults are tuned for
/// a LAN cluster of up to 10,000 nodes: 500ms probe interval, 200ms
/// probe timeout, 5s suspicion window, 10s anti-entropy push-pull.
use std::time::Duration;

/// Configuration for the Mustard gossip protocol.
#[derive(Debug, Clone)]
pub struct GossipConfig {
    /// How often each node runs a probe cycle.
    pub protocol_interval: Duration,
    /// How long to wait for a direct PING-ACK before trying indirect probes.
    pub probe_timeout: Duration,
    /// How long a node stays in Suspect before being declared Dead.
    pub suspicion_timeout: Duration,
    /// Number of indirect probe relays to use on PING timeout.
    pub indirect_probe_count: usize,
    /// How long Dead/Left nodes stay in the membership table before removal.
    pub cleanup_timeout: Duration,
    /// How often a node exchanges its membership table with one random
    /// live peer (anti-entropy push-pull). Piggybacked updates are spent
    /// after a bounded number of re-broadcasts; this periodic resync is
    /// what repairs a node that missed every one of them.
    pub push_pull_interval: Duration,
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            protocol_interval: Duration::from_millis(500),
            probe_timeout: Duration::from_millis(200),
            suspicion_timeout: Duration::from_secs(5),
            indirect_probe_count: 3,
            cleanup_timeout: Duration::from_secs(60),
            // Twenty probe periods. An exchange is capped at
            // `MAX_SYNC_DATAGRAMS` each way whatever the cluster size, so
            // unlike memberlist (whose TCP exchange grows with N and whose
            // interval scales up to compensate) the cost per node is flat:
            // at most 16 datagrams per 10 s, well under the probe traffic.
            push_pull_interval: Duration::from_secs(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = GossipConfig::default();
        assert_eq!(cfg.protocol_interval, Duration::from_millis(500));
        assert_eq!(cfg.probe_timeout, Duration::from_millis(200));
        assert_eq!(cfg.suspicion_timeout, Duration::from_secs(5));
        assert_eq!(cfg.indirect_probe_count, 3);
        assert_eq!(cfg.cleanup_timeout, Duration::from_secs(60));
        assert_eq!(cfg.push_pull_interval, Duration::from_secs(10));
    }
}
