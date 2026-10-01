//! The status screen on an appliance's console (tty1): the only on-box UI.
//!
//! A machine with a monitor plugged in shows who it is and whether bun is
//! up, refreshed every few seconds, so whoever stands in front of a box can
//! tell its name and address without logging in (there's no login).
//! `bun appliance console` runs it; [`render`] is the pure part.

use std::fmt::Write as _;

/// What the screen shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub os_version: Option<String>,
    pub bun_version: Option<String>,
    pub node: Option<String>,
    pub cluster: Option<String>,
    pub address: Option<String>,
    pub macs: Vec<String>,
    /// From bun's `/v1/health`: `None` when bun doesn't answer.
    pub healthy: Option<bool>,
    /// Lines from `bun appliance prepare`, when it's still working (an
    /// unseeded node, or one enrolling).
    pub notes: Vec<String>,
}

/// The screen, for a terminal `width` columns wide: a clear-screen escape,
/// then a short block of `label  value` lines.
pub fn render(status: &Status, width: usize) -> String {
    let unknown = "-".to_string();
    let mut screen = String::from("\x1b[2J\x1b[H");
    let title = format!(
        "Reliaburger OS {}",
        status.os_version.as_deref().unwrap_or("")
    );
    let _ = writeln!(
        screen,
        "{}\n{}\n",
        title.trim_end(),
        "=".repeat(title.trim_end().len().min(width))
    );
    let bun = match status.healthy {
        Some(true) => "running",
        Some(false) => "not healthy",
        None => "not answering",
    };
    let rows = [
        (
            "Node",
            status
                .node
                .clone()
                .unwrap_or_else(|| "not seeded yet".into()),
        ),
        (
            "Cluster",
            status.cluster.clone().unwrap_or_else(|| unknown.clone()),
        ),
        (
            "Address",
            status
                .address
                .clone()
                .unwrap_or_else(|| "waiting for DHCP".into()),
        ),
        (
            "MAC",
            if status.macs.is_empty() {
                unknown.clone()
            } else {
                status.macs.join(" ")
            },
        ),
        (
            "bun",
            format!("{bun} ({})", status.bun_version.as_deref().unwrap_or("-")),
        ),
    ];
    for (label, value) in rows {
        let line = format!("  {label:<9}{value}");
        let _ = writeln!(screen, "{}", truncate(&line, width));
    }
    if !status.notes.is_empty() {
        screen.push('\n');
        for note in &status.notes {
            let _ = writeln!(screen, "{}", truncate(&format!("  {note}"), width));
        }
    }
    if status.node.is_none() {
        let hint = "  Seed this machine: plug in its RBSEED stick and power-cycle it.";
        let _ = writeln!(screen, "\n{}", truncate(hint, width));
    }
    screen
}

/// Read what the screen shows from this machine.
pub async fn gather(paths: &super::Paths) -> Status {
    let os_release = std::fs::read_to_string("/usr/lib/os-release").unwrap_or_default();
    let os_version = os_release
        .lines()
        .find_map(|line| line.strip_prefix("IMAGE_VERSION="))
        .map(|v| v.trim_matches('"').to_string());
    let node_config = std::fs::read_to_string(paths.config_dir.join("node.toml"))
        .ok()
        .and_then(|text| toml::from_str::<crate::config::node::NodeConfig>(&text).ok());
    let bun_version = std::process::Command::new("/var/lib/reliaburger/bin/bun")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|line| line.split_whitespace().nth(1).map(str::to_string));
    Status {
        os_version,
        bun_version,
        node: node_config.as_ref().and_then(|c| c.node.name.clone()),
        cluster: node_config.as_ref().map(|c| c.cluster.name.clone()),
        address: super::address::detect().map(|a| a.to_string()),
        macs: super::prepare::stick_names(&paths.net_dir)
            .into_iter()
            .map(|name| name.replace('-', ":"))
            .collect(),
        healthy: healthy().await,
        notes: Vec::new(),
    }
}

/// bun's local `/v1/health`, over plain HTTP or the node's own TLS.
async fn healthy() -> Option<bool> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;
    for url in [
        "http://127.0.0.1:9117/v1/health",
        "https://127.0.0.1:9117/v1/health",
    ] {
        if let Ok(response) = client.get(url).send().await {
            return Some(response.status().is_success());
        }
    }
    None
}

/// Redraw `tty` every five seconds, for ever.
pub async fn run(paths: &super::Paths, tty: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    loop {
        let screen = render(&gather(paths).await, 80);
        std::fs::OpenOptions::new()
            .write(true)
            .open(tty)?
            .write_all(screen.as_bytes())?;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

fn truncate(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        line.to_string()
    } else {
        line.chars()
            .take(width.saturating_sub(1))
            .chain(['…'])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_node_shows_its_name_address_and_bun() {
        let status = Status {
            os_version: Some("2026.41.0".into()),
            bun_version: Some("0.1.3".into()),
            node: Some("home-2".into()),
            cluster: Some("home".into()),
            address: Some("192.168.1.52".into()),
            macs: vec!["d8:9e:f3:12:34:56".into()],
            healthy: Some(true),
            notes: vec![],
        };
        let screen = render(&status, 80);
        assert!(screen.starts_with("\x1b[2J\x1b[H"));
        for expected in [
            "Reliaburger OS 2026.41.0",
            "  Node     home-2",
            "  Cluster  home",
            "  Address  192.168.1.52",
            "  MAC      d8:9e:f3:12:34:56",
            "  bun      running (0.1.3)",
        ] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
        assert!(!screen.contains("Seed this machine"));
    }

    #[test]
    fn an_unseeded_node_says_how_to_seed_it() {
        let screen = render(&Status::default(), 80);
        assert!(screen.contains("not seeded yet"));
        assert!(screen.contains("waiting for DHCP"));
        assert!(screen.contains("not answering"));
        assert!(screen.contains("Seed this machine"));
    }

    #[test]
    fn long_lines_fit_the_terminal() {
        let status = Status {
            notes: vec!["x".repeat(200)],
            ..Status::default()
        };
        for line in render(&status, 40).lines() {
            assert!(line.chars().count() <= 40, "{line}");
        }
    }
}
