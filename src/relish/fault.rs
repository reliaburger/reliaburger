/// `relish fault` — CLI fault injection commands (Smoker).
///
/// Each subcommand maps to a `FaultRequest` that is sent to the agent
/// via the HTTP API. The agent evaluates safety rails and activates
/// the fault if approved.
use std::time::Duration;

use super::RelishError;
use super::client::BunClient;
use crate::smoker::types::{FaultRequest, FaultType};

/// Default fault duration when --duration is omitted.
const DEFAULT_DURATION: Duration = Duration::from_secs(600); // 10 minutes

/// Parse a duration string like "5s", "30s", "5m", "1h".
fn parse_duration(s: &str) -> Result<Duration, RelishError> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("ms") {
        let ms: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid duration: {s}"),
        })?;
        return Ok(Duration::from_millis(ms));
    }
    if let Some(rest) = s.strip_suffix('s') {
        let secs: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid duration: {s}"),
        })?;
        return Ok(Duration::from_secs(secs));
    }
    if let Some(rest) = s.strip_suffix('m') {
        let mins: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid duration: {s}"),
        })?;
        return Ok(Duration::from_secs(mins * 60));
    }
    if let Some(rest) = s.strip_suffix('h') {
        let hours: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid duration: {s}"),
        })?;
        return Ok(Duration::from_secs(hours * 3600));
    }
    // Try parsing as plain seconds
    let secs: u64 = s.parse().map_err(|_| RelishError::ApiError {
        status: 0,
        body: format!("invalid duration: {s} (try e.g. '30s', '5m', '1h')"),
    })?;
    Ok(Duration::from_secs(secs))
}

/// Parse a delay string like "200ms", "1s" into nanoseconds.
fn parse_delay_ns(s: &str) -> Result<u64, RelishError> {
    let d = parse_duration(s)?;
    Ok(d.as_nanos() as u64)
}

/// Parse a percentage string like "10%" into a u8.
fn parse_percentage(s: &str) -> Result<u8, RelishError> {
    let s = s.trim().trim_end_matches('%');
    let p: u8 = s.parse().map_err(|_| RelishError::ApiError {
        status: 0,
        body: format!("invalid percentage: {s}"),
    })?;
    if p > 100 {
        return Err(RelishError::ApiError {
            status: 0,
            body: format!("percentage must be 0-100, got {p}"),
        });
    }
    Ok(p)
}

/// Parse a bandwidth string like "1mbps", "10mbps" into bytes/sec.
fn parse_bandwidth(s: &str) -> Result<u64, RelishError> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("mbps") {
        let mbps: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid bandwidth: {s}"),
        })?;
        return Ok(mbps * 1024 * 1024);
    }
    if let Some(rest) = s.strip_suffix("kbps") {
        let kbps: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid bandwidth: {s}"),
        })?;
        return Ok(kbps * 1024);
    }
    Err(RelishError::ApiError {
        status: 0,
        body: format!("invalid bandwidth: {s} (try e.g. '1mbps', '100kbps')"),
    })
}

/// Get the duration from an optional --duration flag, defaulting to 10 minutes.
fn get_duration(d: &Option<String>) -> Result<Duration, RelishError> {
    match d {
        Some(s) => parse_duration(s),
        None => {
            eprintln!("Warning: No --duration specified. Fault will auto-expire in 10 minutes.");
            Ok(DEFAULT_DURATION)
        }
    }
}

/// Inject a delay fault.
pub async fn delay(
    target: &str,
    delay_str: &str,
    jitter: Option<&str>,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let delay_ns = parse_delay_ns(delay_str)?;
    let jitter_ns = match jitter {
        Some(j) => parse_delay_ns(j)?,
        None => 0,
    };
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::Delay {
            delay_ns,
            jitter_ns,
        },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a drop fault.
pub async fn drop_fault(
    target: &str,
    percentage_str: &str,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let probability = parse_percentage(percentage_str)?;
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::Drop { probability },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a DNS NXDOMAIN fault.
pub async fn dns(
    target: &str,
    _fault_type: &str,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::DnsNxdomain,
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a partition fault.
pub async fn partition(
    target: &str,
    from: Option<&str>,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::Partition {
            source_app: from.map(|s| s.to_string()),
            source_cgroup_id: 0,
        },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a bandwidth fault.
pub async fn bandwidth(
    target: &str,
    limit_str: &str,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let bytes_per_sec = parse_bandwidth(limit_str)?;
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::Bandwidth { bytes_per_sec },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a CPU stress fault.
pub async fn cpu(
    target: &str,
    percentage_str: &str,
    cores: Option<u32>,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let percentage = parse_percentage(percentage_str)?;
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::CpuStress { percentage, cores },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a memory pressure fault.
pub async fn memory(
    target: &str,
    value: &str,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;
    let (percentage, oom) = if value.trim().eq_ignore_ascii_case("oom") {
        (0, true)
    } else {
        (parse_percentage(value)?, false)
    };

    let request = FaultRequest {
        fault_type: FaultType::MemoryPressure { percentage, oom },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Inject a disk I/O throttle fault.
pub async fn disk_io(
    target: &str,
    limit_str: &str,
    write_only: bool,
    duration: &Option<String>,
) -> Result<(), RelishError> {
    let bytes_per_sec = parse_bandwidth(limit_str)?;
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::DiskIoThrottle {
            bytes_per_sec,
            write_only,
        },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Kill instances of a service.
pub async fn kill(target: &str, count: u32) -> Result<(), RelishError> {
    let request = FaultRequest {
        fault_type: FaultType::Kill { count },
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(0), // Kill is instantaneous
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Pause (freeze) instances of a service.
pub async fn pause(target: &str, duration: &Option<String>) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::Pause,
        target_service: target.into(),
        target_instance: None,
        target_node: None,
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Simulate graceful node departure.
pub async fn node_drain(
    target: &str,
    duration: &Option<String>,
    include_leader: bool,
) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::NodeDrain,
        target_service: "".into(),
        target_instance: None,
        target_node: Some(target.into()),
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// Simulate abrupt node failure.
pub async fn node_kill(
    target: &str,
    duration: &Option<String>,
    kill_containers: bool,
    include_leader: bool,
) -> Result<(), RelishError> {
    let dur = get_duration(duration)?;

    let request = FaultRequest {
        fault_type: FaultType::NodeKill { kill_containers },
        target_service: "".into(),
        target_instance: None,
        target_node: Some(target.into()),
        duration: dur,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: None,
        include_leader,
        override_safety: false,
    };

    inject_and_print(&request).await
}

/// List all active faults.
pub async fn list() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let faults = client.list_faults().await?;

    if faults.is_empty() {
        println!("No active faults");
        return Ok(());
    }

    println!(
        "{:<6} {:<25} {:<15} {:<10} INJECTED BY",
        "ID", "TYPE", "TARGET", "REMAINING"
    );
    for f in &faults {
        println!(
            "{:<6} {:<25} {:<15} {:<10} {}",
            f.id,
            f.fault_type,
            f.target_service,
            format!("{}s", f.remaining_secs),
            f.injected_by,
        );
    }
    println!();
    println!("{} active fault(s)", faults.len());
    Ok(())
}

/// Clear faults — all or by ID.
pub async fn clear(id: Option<u64>) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let msg = match id {
        Some(fault_id) => client.clear_fault(fault_id).await?,
        None => client.clear_all_faults().await?,
    };
    println!("{msg}");
    Ok(())
}

/// Submit a fault request and print the result.
async fn inject_and_print(request: &FaultRequest) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let summary = client.inject_fault(request).await?;
    println!(
        "Fault injected: {} on {} (id: {}, expires in {}s)",
        summary.fault_type, summary.target_service, summary.id, summary.remaining_secs,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
    }

    #[test]
    fn parse_duration_plain_number_is_seconds() {
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn parse_delay_ns_200ms() {
        assert_eq!(parse_delay_ns("200ms").unwrap(), 200_000_000);
    }

    #[test]
    fn parse_delay_ns_1s() {
        assert_eq!(parse_delay_ns("1s").unwrap(), 1_000_000_000);
    }

    #[test]
    fn parse_percentage_10() {
        assert_eq!(parse_percentage("10%").unwrap(), 10);
    }

    #[test]
    fn parse_percentage_100() {
        assert_eq!(parse_percentage("100%").unwrap(), 100);
    }

    #[test]
    fn parse_percentage_no_sign() {
        assert_eq!(parse_percentage("50").unwrap(), 50);
    }

    #[test]
    fn parse_percentage_over_100_rejected() {
        assert!(parse_percentage("101%").is_err());
    }

    #[test]
    fn parse_bandwidth_mbps() {
        assert_eq!(parse_bandwidth("1mbps").unwrap(), 1024 * 1024);
    }

    #[test]
    fn parse_bandwidth_kbps() {
        assert_eq!(parse_bandwidth("100kbps").unwrap(), 100 * 1024);
    }

    #[test]
    fn parse_bandwidth_invalid() {
        assert!(parse_bandwidth("fast").is_err());
    }
}
