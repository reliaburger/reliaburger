/// Gossip protocol configuration.
///
/// Timing parameters for the SWIM probe cycle. Defaults are tuned for
/// a LAN cluster of up to 10,000 nodes: 500ms probe interval, 200ms
/// probe timeout, 5s suspicion window.
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
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            protocol_interval: Duration::from_millis(500),
            probe_timeout: Duration::from_millis(200),
            suspicion_timeout: Duration::from_secs(5),
            indirect_probe_count: 3,
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
    }
}
