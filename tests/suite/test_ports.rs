//! How test processes share one host's ports without colliding (#2261).
//!
//! Every fixture port comes from `TEST_PORTS` under a file lease, which keeps
//! other leasing fixtures off it. These tests pin the two ways a held port
//! used to be taken anyway: a helper that dropped its lease before Bun bound
//! the port, and a test Bun handing a workload a port from the lease range.

use reliaburger::config::node::NodeConfig;

use crate::bun_process::{
    PortBlockReservation, TEST_PORTS, TEST_WORKLOAD_PORTS, reserve_address, reserve_port_block,
    reserve_ports, spawn_bun_with_port_retry, use_test_workload_ports, write_portable_node_config,
};

/// Linux hands out ephemeral ports from 32768 (`net.ipv4.ip_local_port_range`).
const LINUX_EPHEMERAL_START: u16 = 32768;

/// The environment variable that turns this test binary into the child that
/// tries to take a port another process holds.
const CHILD_PORT: &str = "RELIABURGER_TEST_PORTS_CHILD_PORT";

/// A port `reserve_address` hands out stays leased after it returns, so
/// another test process can't take it before this test's Bun binds it. The
/// helper used to drop its lease as it returned; a leasing fixture elsewhere
/// then took the same port and whichever bound second failed.
#[test]
fn a_reserved_address_stays_leased_against_other_processes_until_the_test_exits() {
    if let Ok(port) = std::env::var(CHILD_PORT) {
        let port: u16 = port.parse().unwrap();
        assert!(
            PortBlockReservation::try_reserve(port, 1).is_none(),
            "another process leased {port} while its holder was still running"
        );
        return;
    }
    let address = reserve_address();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "test_ports::a_reserved_address_stays_leased_against_other_processes_until_the_test_exits",
            "--nocapture",
        ])
        .env(CHILD_PORT, address.port().to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The child ran the check, rather than matching no test at all.
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// `reserve_ports` and `reserve_port_block` keep every port's lease too.
#[test]
fn reserved_ports_and_blocks_stay_leased_until_the_test_exits() {
    for port in reserve_ports() {
        assert!(PortBlockReservation::try_reserve(port, 1).is_none());
    }
    let base = reserve_port_block(4);
    for port in base..base + 4 {
        assert!(PortBlockReservation::try_reserve(port, 1).is_none());
    }
}

// Workload host ports come from a range that shares no port with the lease
// range and lies below every ephemeral range. Checked when this compiles.
const _: () = {
    assert!(TEST_WORKLOAD_PORTS.start < TEST_WORKLOAD_PORTS.end);
    assert!(
        TEST_WORKLOAD_PORTS.end <= TEST_PORTS.start || TEST_WORKLOAD_PORTS.start >= TEST_PORTS.end,
        "TEST_WORKLOAD_PORTS overlaps the lease range"
    );
    assert!(TEST_WORKLOAD_PORTS.end <= LINUX_EPHEMERAL_START);
};

fn assert_outside_the_lease_range(config: &NodeConfig) {
    let range = &config.network.port_range;
    assert!(
        range.end <= TEST_PORTS.start || range.start >= TEST_PORTS.end,
        "a test Bun's port_range {}..{} overlaps the lease range {TEST_PORTS:?}",
        range.start,
        range.end
    );
}

/// A test node config, once prepared for Bun, never lets Bun hand a workload
/// a leased port. Bun's default range, 10000–32767, covers all of them.
#[test]
fn a_prepared_test_node_config_keeps_workload_ports_out_of_the_lease_range() {
    let root = tempfile::tempdir().unwrap();
    let path = write_portable_node_config(root.path());
    use_test_workload_ports(&path);
    let config = NodeConfig::from_file(&path).unwrap();
    assert_eq!(config.network.port_range.start, TEST_WORKLOAD_PORTS.start);
    assert_eq!(config.network.port_range.end, TEST_WORKLOAD_PORTS.end);
    assert_outside_the_lease_range(&config);
    // Its other settings survive the edit.
    assert_eq!(
        config.network.advertise_address.as_deref(),
        Some("127.0.0.1")
    );
}

/// A config that has no `[network]` table gets one, and one that sets its
/// own range keeps it.
#[test]
fn preparing_a_config_adds_a_network_table_and_keeps_an_explicit_range() {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("bare.toml");
    std::fs::write(
        &bare,
        "# a comment worth keeping\n[node]\nname = \"bare\"\n",
    )
    .unwrap();
    use_test_workload_ports(&bare);
    let text = std::fs::read_to_string(&bare).unwrap();
    assert!(text.contains("# a comment worth keeping"), "{text}");
    let config = NodeConfig::from_file(&bare).unwrap();
    assert_eq!(config.network.port_range.start, TEST_WORKLOAD_PORTS.start);

    let explicit = root.path().join("explicit.toml");
    std::fs::write(
        &explicit,
        "[network]\nport_range = { start = 40000, end = 40010 }\n",
    )
    .unwrap();
    use_test_workload_ports(&explicit);
    let config = NodeConfig::from_file(&explicit).unwrap();
    assert_eq!(config.network.port_range.start, 40000);
    assert_eq!(config.network.port_range.end, 40010);
}

/// A config that went through `NodeConfig` and back carries Bun's default
/// range, 10000–32767, written out explicitly. That's not a range of the
/// test's own choosing, and it covers the lease range, so it's replaced. The
/// Linux fixtures build most of their configs this way, from
/// `relish init`'s output.
#[test]
fn preparing_a_serialised_config_replaces_a_range_that_covers_the_lease_range() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("node.toml");
    std::fs::write(&path, toml::to_string(&NodeConfig::default()).unwrap()).unwrap();
    let before = NodeConfig::from_file(&path).unwrap();
    assert!(before.network.port_range.start < TEST_PORTS.end);
    assert!(before.network.port_range.end > TEST_PORTS.start);
    use_test_workload_ports(&path);
    let config = NodeConfig::from_file(&path).unwrap();
    assert_eq!(config.network.port_range.start, TEST_WORKLOAD_PORTS.start);
    assert_eq!(config.network.port_range.end, TEST_WORKLOAD_PORTS.end);
}

/// The shared spawn helpers prepare the config themselves, so a test that
/// starts Bun through them can't forget.
#[test]
fn a_bun_started_by_the_shared_helpers_uses_the_test_workload_ports() {
    let root = tempfile::tempdir().unwrap();
    let (bun, _address) = spawn_bun_with_port_retry(false, || {
        (
            write_portable_node_config(root.path()),
            reserve_address(),
            root.path().join("bun.log"),
        )
    });
    let config = NodeConfig::from_file(&root.path().join("node.toml")).unwrap();
    assert_outside_the_lease_range(&config);
    // Bun bound its API with the edited config, so the edit loads.
    drop(bun);
}
