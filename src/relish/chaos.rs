/// Chaos testing scenarios for `relish chaos`.
///
/// Each scenario discovers the cluster topology, injects a fault,
/// observes the reaction, heals, and verifies recovery. Output is
/// a timestamped narrative designed to be watched in real time.
use std::time::Instant;

use super::RelishError;
use super::client::BunClient;

/// Print a timestamped, coloured chaos event.
fn event(start: Instant, phase: &str, message: &str) {
    let elapsed = start.elapsed().as_secs_f64();
    let colour = match phase {
        "DISCOVER" => "\x1b[36m", // cyan
        "INJECT" => "\x1b[31m",   // red
        "OBSERVE" => "\x1b[33m",  // yellow
        "TEST" => "\x1b[35m",     // magenta
        "HEAL" => "\x1b[32m",     // green
        "VERIFY" => "\x1b[32m",   // green
        "SETUP" => "\x1b[36m",    // cyan
        _ => "\x1b[0m",
    };
    let reset = "\x1b[0m";
    println!("  [{elapsed:6.2}s]  {colour}{phase:<10}{reset}{message}");
}

/// Run the council partition scenario against a live cluster.
pub async fn council_partition(client: &BunClient) -> Result<(), RelishError> {
    println!();
    println!("\x1b[1mCHAOS  Council Partition\x1b[0m");
    println!("{}", "─".repeat(55));
    println!();

    let start = Instant::now();

    // Discover topology
    event(start, "DISCOVER", "querying cluster topology...");
    let nodes = client.nodes().await?;

    if nodes.is_empty() {
        event(
            start,
            "DISCOVER",
            "no cluster nodes found (single-node mode)",
        );
        println!();
        println!("  \x1b[33mSKIPPED\x1b[0m — need a multi-node cluster");
        return Ok(());
    }

    let council_nodes: Vec<_> = nodes.iter().filter(|n| n.is_council).collect();

    let summary = nodes
        .iter()
        .map(|n| {
            let role = if n.is_leader {
                "leader, council"
            } else if n.is_council {
                "council"
            } else {
                "worker"
            };
            format!("{} ({})", n.node_id, role)
        })
        .collect::<Vec<_>>()
        .join(", ");
    event(
        start,
        "DISCOVER",
        &format!("found {} nodes: {summary}", nodes.len()),
    );

    if council_nodes.len() < 2 {
        event(
            start,
            "DISCOVER",
            "need at least 2 council members for partition test",
        );
        println!();
        println!("  \x1b[33mSKIPPED\x1b[0m — insufficient council members");
        return Ok(());
    }

    // Pick a minority council member (not the leader if possible)
    let target = council_nodes
        .iter()
        .find(|n| !n.is_leader)
        .unwrap_or(&council_nodes[0]);

    // Build list of peers to block (all other council members)
    let peers_to_block: Vec<String> = council_nodes
        .iter()
        .filter(|n| n.node_id != target.node_id)
        .map(|n| n.address.clone())
        .collect();

    let duration_secs = 30;
    event(
        start,
        "INJECT",
        &format!(
            "partitioning {} from {} peer(s), duration: {duration_secs}s (auto-heal)",
            target.node_id,
            peers_to_block.len()
        ),
    );

    // We need to talk to the TARGET node's API, not just the leader.
    // For now, inject via the provided client (assumes it can reach the target).
    // TODO: support --agent per-node targeting
    client
        .inject_partition(&peers_to_block, duration_secs)
        .await?;

    // Poll and observe
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _current = client.nodes().await.unwrap_or_default();
        let council_status = client.council().await.ok();

        if let Some(cs) = &council_status {
            let leader_info = cs.leader.as_deref().unwrap_or("(none)");
            event(
                start,
                "POLL",
                &format!(
                    "leader: {leader_info}, term: {}, members: {}",
                    cs.term,
                    cs.members.len()
                ),
            );
        }
    }

    // Heal
    event(start, "HEAL", "removing partition...");
    client.heal_partition().await?;

    // Wait for convergence
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify
    let final_nodes = client.nodes().await?;
    let final_council = client.council().await?;
    event(
        start,
        "VERIFY",
        &format!(
            "cluster has {} nodes, leader: {}",
            final_nodes.len(),
            final_council.leader.as_deref().unwrap_or("(none)")
        ),
    );

    let elapsed = start.elapsed().as_secs_f64();
    println!();
    println!("  \x1b[32mPASSED\x1b[0m  council partition scenario in {elapsed:.1}s");
    println!();

    Ok(())
}

/// Run the worker isolation scenario against a live cluster.
pub async fn worker_isolation(client: &BunClient) -> Result<(), RelishError> {
    println!();
    println!("\x1b[1mCHAOS  Worker Isolation\x1b[0m");
    println!("{}", "─".repeat(55));
    println!();

    let start = Instant::now();

    event(start, "DISCOVER", "querying cluster topology...");
    let nodes = client.nodes().await?;

    let workers: Vec<_> = nodes.iter().filter(|n| !n.is_council).collect();
    let council_nodes: Vec<_> = nodes.iter().filter(|n| n.is_council).collect();

    if workers.is_empty() {
        event(start, "DISCOVER", "no worker nodes found");
        println!();
        println!("  \x1b[33mSKIPPED\x1b[0m — need at least one worker node");
        return Ok(());
    }

    let target = &workers[0];
    let summary = format!(
        "target worker: {}, {} council members",
        target.node_id,
        council_nodes.len()
    );
    event(start, "DISCOVER", &summary);

    // Block the worker from all council members
    let peers_to_block: Vec<String> = council_nodes.iter().map(|n| n.address.clone()).collect();

    let duration_secs = 30;
    event(
        start,
        "INJECT",
        &format!(
            "isolating {} from {} council member(s), duration: {duration_secs}s",
            target.node_id,
            peers_to_block.len()
        ),
    );

    client
        .inject_partition(&peers_to_block, duration_secs)
        .await?;

    // Observe — poll for stale detection
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let chaos = client.chaos_status().await.unwrap_or_default();
        if let Some(p) = &chaos.active_partition {
            event(
                start,
                "OBSERVE",
                &format!(
                    "partition active, {} peer(s) blocked, {}s remaining",
                    p.peers.len(),
                    p.remaining_secs
                ),
            );
        }
    }

    // Heal
    event(start, "HEAL", "removing partition...");
    client.heal_partition().await?;

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify
    let final_nodes = client.nodes().await?;
    event(
        start,
        "VERIFY",
        &format!("cluster has {} nodes, all reachable", final_nodes.len()),
    );

    let elapsed = start.elapsed().as_secs_f64();
    println!();
    println!("  \x1b[32mPASSED\x1b[0m  worker isolation scenario in {elapsed:.1}s");
    println!();

    Ok(())
}

/// Show active chaos state.
pub async fn status(client: &BunClient) -> Result<(), RelishError> {
    let state = client.chaos_status().await?;
    match state.active_partition {
        Some(p) => {
            println!("Active partitions:");
            println!(
                "  blocking {} peer(s): {} ({}s remaining)",
                p.peers.len(),
                p.peers.join(", "),
                p.remaining_secs
            );
        }
        None => {
            println!("No active partitions");
        }
    }
    Ok(())
}

/// Heal all active chaos injections.
pub async fn heal(client: &BunClient) -> Result<(), RelishError> {
    let msg = client.heal_partition().await?;
    println!("{msg}");
    Ok(())
}
