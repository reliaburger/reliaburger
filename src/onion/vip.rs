/// Virtual IP allocation for Onion service discovery.
///
/// Each service gets a deterministic virtual IP from the
/// `127.128.0.0/16` range. The VIP is derived from the app name
/// using SipHash-2-4 with fixed seeds, so the same app name always
/// maps to the same VIP cluster-wide. No coordination needed.
///
/// The `127.128.0.0/16` range lives within the loopback block
/// (`127.0.0.0/8`), so it never conflicts with real network
/// addresses. The eBPF `connect()` hook intercepts connections to
/// these addresses and rewrites them to real backends.
use std::hash::{Hash, Hasher};
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use siphasher::sip::SipHasher24;

/// A virtual IP address for a service.
///
/// Deterministically derived from the app name. The same name always
/// produces the same VIP. Lives in the `127.128.0.0/16` range.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct VirtualIP(pub Ipv4Addr);

impl VirtualIP {
    /// Derive a VIP from an app name.
    ///
    /// Uses SipHash-2-4 with fixed seeds to distribute names across
    /// the `127.128.0.1` through `127.128.255.254` range (65,534
    /// usable addresses). Deterministic: same name, same VIP.
    pub fn from_app_name(name: &str) -> Self {
        let mut hasher = SipHasher24::new_with_keys(0xDEAD_BEEF_CAFE_F00D, 0xBAAD_F00D_DEAD_BEEF);
        name.hash(&mut hasher);
        let hash = hasher.finish();

        // Map to range 1..=65534 (skip .0.0 and .255.255)
        let offset = (hash % 65534) as u32 + 1;
        let ip = 0x7F80_0000u32 | (offset & 0xFFFF);
        VirtualIP(Ipv4Addr::from(ip))
    }

    /// Check whether an IP address is in the VIP range (`127.128.0.0/16`).
    pub fn is_in_vip_range(ip: Ipv4Addr) -> bool {
        let octets = ip.octets();
        octets[0] == 127 && octets[1] == 128
    }

    /// Convert to network byte order (big-endian) u32 for BPF maps.
    pub fn to_network_byte_order(self) -> u32 {
        u32::from(self.0).to_be()
    }

    /// Create from a network byte order u32.
    pub fn from_network_byte_order(nbo: u32) -> Self {
        VirtualIP(Ipv4Addr::from(u32::from_be(nbo)))
    }
}

impl std::fmt::Display for VirtualIP {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for VirtualIP {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for VirtualIP {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let ip: Ipv4Addr = s.parse().map_err(serde::de::Error::custom)?;
        Ok(VirtualIP(ip))
    }
}

/// Compute a deterministic u32 identifier from a name.
///
/// Used for `app_id` and `namespace_id` fields in BPF maps.
/// Same hash function as VIP allocation but with different seeds.
pub fn name_to_id(name: &str) -> u32 {
    let mut hasher = SipHasher24::new_with_keys(0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210);
    name.hash(&mut hasher);
    hasher.finish() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_app_name_deterministic() {
        let a = VirtualIP::from_app_name("redis");
        let b = VirtualIP::from_app_name("redis");
        assert_eq!(a, b);
    }

    #[test]
    fn from_app_name_different_names_different_vips() {
        let redis = VirtualIP::from_app_name("redis");
        let web = VirtualIP::from_app_name("web");
        assert_ne!(redis, web);
    }

    #[test]
    fn from_app_name_in_vip_range() {
        for name in &["redis", "web", "api", "postgres", "worker", "nginx"] {
            let vip = VirtualIP::from_app_name(name);
            assert!(
                VirtualIP::is_in_vip_range(vip.0),
                "{name} produced VIP {vip} outside range"
            );
        }
    }

    #[test]
    fn vip_avoids_zero_and_broadcast() {
        // Run through many names and verify none produce .0.0 or .255.255
        for i in 0..10000 {
            let name = format!("service-{i}");
            let vip = VirtualIP::from_app_name(&name);
            let octets = vip.0.octets();
            let low16 = ((octets[2] as u16) << 8) | octets[3] as u16;
            assert_ne!(low16, 0, "{name} produced .0.0");
            assert_ne!(low16, 0xFFFF, "{name} produced .255.255");
        }
    }

    #[test]
    fn is_in_vip_range_accepts_valid() {
        assert!(VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 128, 0, 1)));
        assert!(VirtualIP::is_in_vip_range(Ipv4Addr::new(
            127, 128, 255, 254
        )));
        assert!(VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 128, 42, 7)));
    }

    #[test]
    fn is_in_vip_range_rejects_localhost() {
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn is_in_vip_range_rejects_external() {
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn is_in_vip_range_rejects_other_loopback() {
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 0, 0, 2)));
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 127, 0, 1)));
        assert!(!VirtualIP::is_in_vip_range(Ipv4Addr::new(127, 129, 0, 1)));
    }

    #[test]
    fn to_network_byte_order_round_trip() {
        let vip = VirtualIP::from_app_name("redis");
        let nbo = vip.to_network_byte_order();
        let back = VirtualIP::from_network_byte_order(nbo);
        assert_eq!(vip, back);
    }

    #[test]
    fn display_shows_ip() {
        let vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 3));
        assert_eq!(vip.to_string(), "127.128.0.3");
    }

    #[test]
    fn serde_round_trip() {
        let vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 42));
        let json = serde_json::to_string(&vip).unwrap();
        assert_eq!(json, "\"127.128.0.42\"");
        let back: VirtualIP = serde_json::from_str(&json).unwrap();
        assert_eq!(vip, back);
    }

    #[test]
    fn name_to_id_deterministic() {
        let a = name_to_id("default");
        let b = name_to_id("default");
        assert_eq!(a, b);
    }

    #[test]
    fn name_to_id_different_names() {
        let a = name_to_id("default");
        let b = name_to_id("production");
        assert_ne!(a, b);
    }
}
