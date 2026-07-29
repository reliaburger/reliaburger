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
pub fn parse_duration(s: &str) -> Result<Duration, RelishError> {
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
pub fn parse_delay_ns(s: &str) -> Result<u64, RelishError> {
    let d = parse_duration(s)?;
    Ok(d.as_nanos() as u64)
}

/// Parse a percentage string like "10%" into a u8.
pub fn parse_percentage(s: &str) -> Result<u8, RelishError> {
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
///
/// The `bps` suffix means bits per second (O17): `1mbps` is one megabit/s =
/// 1_000_000 / 8 = 125_000 bytes/s, not one mebibyte/s. The old code multiplied
/// by 1024×1024, over-provisioning the throttle ~8.4×.
pub fn parse_bandwidth(s: &str) -> Result<u64, RelishError> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("mbps") {
        let mbps: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid bandwidth: {s}"),
        })?;
        // megabits/s → bytes/s
        return Ok(mbps * 1_000_000 / 8);
    }
    if let Some(rest) = s.strip_suffix("kbps") {
        let kbps: u64 = rest.parse().map_err(|_| RelishError::ApiError {
            status: 0,
            body: format!("invalid bandwidth: {s}"),
        })?;
        // kilobits/s → bytes/s
        return Ok(kbps * 1000 / 8);
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

/// Flags shared by every fault subcommand for narrowing and annotating a
/// fault. These were on `FaultRequest` all along but hardcoded `None`/`false`
/// in the CLI; now they reach the wire.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct FaultTargeting {
    /// Scope the fault to a single instance id instead of every instance.
    #[arg(long)]
    pub instance: Option<String>,
    /// Route the fault to a specific node (leader-mediated distribution).
    #[arg(long)]
    pub node: Option<String>,
    /// A human reason recorded alongside the fault.
    #[arg(long)]
    pub reason: Option<String>,
    /// Override the node-percentage safety rail (the one rail designed to be
    /// overridable).
    #[arg(long)]
    pub override_safety: bool,
}

/// Build a `FaultRequest`, filling the common fields from `targeting`.
fn make_request(
    fault_type: FaultType,
    target_service: String,
    duration: Duration,
    targeting: &FaultTargeting,
) -> FaultRequest {
    FaultRequest {
        fault_type,
        target_service,
        target_instance: targeting.instance.clone(),
        target_node: targeting.node.clone(),
        duration,
        injected_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        reason: targeting.reason.clone(),
        include_leader: false,
        override_safety: targeting.override_safety,
        acknowledged: false,
    }
}

/// Inject a delay fault.
pub async fn delay(
    target: &str,
    delay_str: &str,
    jitter: Option<&str>,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let delay_ns = parse_delay_ns(delay_str)?;
    let jitter_ns = match jitter {
        Some(j) => parse_delay_ns(j)?,
        None => 0,
    };
    let request = make_request(
        FaultType::Delay {
            delay_ns,
            jitter_ns,
        },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a drop fault.
pub async fn drop_fault(
    target: &str,
    percentage_str: &str,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let probability = parse_percentage(percentage_str)?;
    let request = make_request(
        FaultType::Drop { probability },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a DNS fault. Only `nxdomain` is supported; an unknown type is
/// rejected rather than silently injecting NXDOMAIN.
pub async fn dns(
    target: &str,
    fault_type: &str,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    if !fault_type.eq_ignore_ascii_case("nxdomain") {
        return Err(RelishError::ApiError {
            status: 0,
            body: format!("unknown dns fault type {fault_type:?}; supported: nxdomain"),
        });
    }
    let request = make_request(
        FaultType::DnsNxdomain,
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a partition fault.
pub async fn partition(
    target: &str,
    from: Option<&str>,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let request = make_request(
        FaultType::Partition {
            source_app: from.map(|s| s.to_string()),
            source_cgroup_id: 0,
        },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a bandwidth fault.
pub async fn bandwidth(
    target: &str,
    limit_str: &str,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let bytes_per_sec = parse_bandwidth(limit_str)?;
    let request = make_request(
        FaultType::Bandwidth { bytes_per_sec },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a CPU stress fault.
pub async fn cpu(
    target: &str,
    percentage_str: &str,
    cores: Option<u32>,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let percentage = parse_percentage(percentage_str)?;
    let request = make_request(
        FaultType::CpuStress { percentage, cores },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a memory pressure fault.
pub async fn memory(
    target: &str,
    value: &str,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let (percentage, oom) = if value.trim().eq_ignore_ascii_case("oom") {
        (0, true)
    } else {
        (parse_percentage(value)?, false)
    };
    let request = make_request(
        FaultType::MemoryPressure { percentage, oom },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Inject a disk I/O throttle fault.
pub async fn disk_io(
    target: &str,
    limit_str: &str,
    write_only: bool,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let bytes_per_sec = parse_bandwidth(limit_str)?;
    let request = make_request(
        FaultType::DiskIoThrottle {
            bytes_per_sec,
            write_only,
        },
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Kill instances of a service.
pub async fn kill(target: &str, count: u32, targeting: &FaultTargeting) -> Result<(), RelishError> {
    // Kill is instantaneous.
    let request = make_request(
        FaultType::Kill { count },
        target.into(),
        Duration::from_secs(0),
        targeting,
    );
    inject_and_print(&request).await
}

/// Pause (freeze) instances of a service.
pub async fn pause(
    target: &str,
    duration: &Option<String>,
    targeting: &FaultTargeting,
) -> Result<(), RelishError> {
    let request = make_request(
        FaultType::Pause,
        target.into(),
        get_duration(duration)?,
        targeting,
    );
    inject_and_print(&request).await
}

/// Resume (unfreeze) previously paused instances of a service.
pub async fn resume(target: &str, targeting: &FaultTargeting) -> Result<(), RelishError> {
    let request = make_request(
        FaultType::Resume,
        target.into(),
        Duration::from_secs(0),
        targeting,
    );
    inject_and_print(&request).await
}

/// Simulate graceful node departure.
pub async fn node_drain(
    target: &str,
    duration: &Option<String>,
    include_leader: bool,
    reason: Option<&str>,
    override_safety: bool,
    acknowledged: bool,
) -> Result<(), RelishError> {
    let targeting = FaultTargeting {
        node: Some(target.into()),
        reason: reason.map(str::to_string),
        override_safety,
        ..FaultTargeting::default()
    };
    let mut request = make_request(
        FaultType::NodeDrain,
        String::new(),
        get_duration(duration)?,
        &targeting,
    );
    request.include_leader = include_leader;
    request.acknowledged = acknowledged;
    inject_and_print(&request).await
}

/// Simulate abrupt node failure.
pub async fn node_kill(
    target: &str,
    duration: &Option<String>,
    kill_containers: bool,
    include_leader: bool,
    reason: Option<&str>,
    override_safety: bool,
    acknowledged: bool,
) -> Result<(), RelishError> {
    let targeting = FaultTargeting {
        node: Some(target.into()),
        reason: reason.map(str::to_string),
        override_safety,
        ..FaultTargeting::default()
    };
    let mut request = make_request(
        FaultType::NodeKill { kill_containers },
        String::new(),
        get_duration(duration)?,
        &targeting,
    );
    request.include_leader = include_leader;
    request.acknowledged = acknowledged;
    inject_and_print(&request).await
}

/// Consume bounded CPU and memory capacity on one node.
#[allow(clippy::too_many_arguments)]
pub async fn node_pressure(
    target: &str,
    cpu: &str,
    memory: &str,
    duration: &Option<String>,
    include_leader: bool,
    reason: Option<&str>,
    override_safety: bool,
    acknowledged: bool,
) -> Result<(), RelishError> {
    let cpu_percentage = parse_percentage(cpu)?;
    let memory_percentage = parse_percentage(memory)?;
    let targeting = FaultTargeting {
        node: Some(target.into()),
        reason: reason.map(str::to_string),
        override_safety,
        ..FaultTargeting::default()
    };
    let mut request = make_request(
        FaultType::NodePressure {
            cpu_percentage,
            memory_percentage,
        },
        String::new(),
        get_duration(duration)?,
        &targeting,
    );
    request.include_leader = include_leader;
    request.acknowledged = acknowledged;
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
        "{:<6} {:<22} {:<15} {:<15} {:<10} INJECTED BY",
        "ID", "TYPE", "TARGET", "INSTANCE", "REMAINING"
    );
    for f in &faults {
        println!(
            "{:<6} {:<22} {:<15} {:<15} {:<10} {}",
            f.id,
            f.fault_type,
            f.target_node.as_deref().unwrap_or(&f.target_service),
            f.target_instance.as_deref().unwrap_or("-"),
            format!("{}s", f.remaining_secs),
            f.injected_by,
        );
    }
    println!();
    println!("{} active fault(s)", faults.len());
    Ok(())
}

/// Clear faults — all, by numeric id, or by app name.
///
/// A bare number is a fault id; anything else is a service name (its first
/// caller for the registry's `clear_by_service`). No argument clears all.
pub async fn clear(
    target: Option<String>,
    node: Option<&str>,
    acknowledged: bool,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let msg = match target {
        None => client.clear_all_faults().await?,
        Some(arg) => match arg.parse::<u64>() {
            Ok(id) => client.clear_fault(id, node, acknowledged).await?,
            Err(_) if node.is_some() => {
                return Err(RelishError::ApiError {
                    status: 0,
                    body: "--node can only be used when clearing a numeric fault id".to_string(),
                });
            }
            Err(_) => client.clear_faults_by_service(&arg).await?,
        },
    };
    println!("{msg}");
    Ok(())
}

/// Run a scripted chaos scenario from a TOML file.
pub async fn scenario(
    path: &std::path::Path,
    dry_run: bool,
    speed: f64,
) -> Result<(), RelishError> {
    use crate::smoker::scenario;

    let s = scenario::load_scenario(path)?;
    let timeline = scenario::build_timeline(&s, speed)?;

    if dry_run {
        scenario::print_dry_run(&s, &timeline);
        return Ok(());
    }

    println!("Executing scenario: {}", s.name);
    let start = std::time::Instant::now();
    let client = BunClient::default_local();

    for entry in &timeline {
        // Wait until activation time
        let elapsed = start.elapsed();
        if entry.activation > elapsed {
            let wait = entry.activation - elapsed;
            println!("  Waiting {:?} for next step...", wait);
            tokio::time::sleep(wait).await;
        }

        println!(
            "  T+{:.0}s: {} -- {} {} {}",
            start.elapsed().as_secs_f64(),
            entry.step.description,
            entry.step.fault,
            entry.step.target,
            entry.step.value,
        );

        let req = scenario::step_to_fault_request(&entry.step, speed)?;
        match client.inject_fault(&req).await {
            Ok(summary) => {
                println!(
                    "    -> injected (id: {}, expires in {}s)",
                    summary.id, summary.remaining_secs
                );
            }
            Err(e) => {
                println!("    -> FAILED: {e}");
            }
        }
    }

    println!();
    println!(
        "Scenario complete: {} steps in {:.1}s",
        timeline.len(),
        start.elapsed().as_secs_f64()
    );
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
        // 1 megabit/s = 1_000_000 bits / 8 = 125_000 bytes/s (O17).
        assert_eq!(parse_bandwidth("1mbps").unwrap(), 125_000);
    }

    #[test]
    fn parse_bandwidth_kbps() {
        // 100 kilobits/s = 100_000 bits / 8 = 12_500 bytes/s.
        assert_eq!(parse_bandwidth("100kbps").unwrap(), 12_500);
    }

    #[test]
    fn parse_bandwidth_invalid() {
        assert!(parse_bandwidth("fast").is_err());
    }

    #[test]
    fn make_request_carries_instance_node_and_reason() {
        let targeting = FaultTargeting {
            instance: Some("redis-1".into()),
            node: Some("node-2".into()),
            reason: Some("game day".into()),
            override_safety: true,
        };
        let request = make_request(
            FaultType::Pause,
            "redis".into(),
            Duration::from_secs(60),
            &targeting,
        );
        assert_eq!(request.target_service, "redis");
        assert_eq!(request.target_instance.as_deref(), Some("redis-1"));
        assert_eq!(request.target_node.as_deref(), Some("node-2"));
        assert_eq!(request.reason.as_deref(), Some("game day"));
        assert!(request.override_safety);
    }

    /// `dns redis banana` used to silently inject NXDOMAIN; now the positional
    /// is validated before any request is built.
    #[tokio::test]
    async fn dns_rejects_an_unknown_fault_type() {
        let error = dns("redis", "banana", &None, &FaultTargeting::default())
            .await
            .unwrap_err();
        match error {
            RelishError::ApiError { body, .. } => {
                assert!(body.contains("banana"), "{body}");
                assert!(body.contains("nxdomain"), "{body}");
            }
            other => panic!("expected an ApiError, got {other:?}"),
        }
    }
}
