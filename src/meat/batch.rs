//! Batch scheduler — high-throughput bulk job allocation.
//!
//! The per-job Filter→Score→Select→Commit pipeline is designed for
//! long-running apps where quality of placement matters. For batch jobs
//! (short-lived, many identical instances), we need raw throughput.
//!
//! The batch scheduler groups jobs by resource profile, sorts nodes by
//! available capacity, and greedily bin-packs in O(nodes × profiles).
//! This achieves the 100M jobs/day target (~1,157/s sustained).

use std::collections::HashMap;

use super::types::{NodeCapacity, NodeId, Resources};

/// Unique identifier for a batch submission.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatchId(pub u64);

impl std::fmt::Display for BatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "batch-{}", self.0)
    }
}

/// A single job in a batch submission.
#[derive(Debug, Clone)]
pub struct BatchJob {
    /// Job name (for tracking).
    pub name: String,
    /// Resources required by this job.
    pub resources: Resources,
}

/// Result of batch scheduling: which jobs go to which nodes.
#[derive(Debug, Clone)]
pub struct BatchAllocation {
    /// Assigned jobs: (job_name, node_id).
    pub assignments: Vec<(String, NodeId)>,
    /// Jobs that couldn't be placed (insufficient capacity).
    pub unschedulable: Vec<String>,
}

/// Schedule a batch of jobs across available nodes using greedy bin-packing.
///
/// Algorithm:
/// 1. Group jobs by resource profile (identical resource requirements).
/// 2. Sort nodes by available capacity (most capacity first).
/// 3. For each profile group, greedily assign jobs to nodes that can fit them.
///
/// This runs in O(nodes × profiles + total_jobs) time, not O(nodes × jobs).
pub fn schedule_batch(jobs: &[BatchJob], nodes: &mut [NodeCapacity]) -> BatchAllocation {
    if jobs.is_empty() {
        return BatchAllocation {
            assignments: Vec::new(),
            unschedulable: Vec::new(),
        };
    }

    // Group jobs by resource profile
    let mut profile_groups: HashMap<ResourceProfile, Vec<&BatchJob>> = HashMap::new();
    for job in jobs {
        let profile = ResourceProfile::from(&job.resources);
        profile_groups.entry(profile).or_default().push(job);
    }

    let mut assignments = Vec::with_capacity(jobs.len());
    let mut unschedulable = Vec::new();

    // Process each profile group
    for group_jobs in profile_groups.values() {
        let required = &group_jobs[0].resources;

        // Sort nodes by allocatable capacity (descending) for this pass.
        // Nodes with more capacity get jobs first → better packing.
        nodes.sort_by(|a, b| {
            b.allocatable()
                .cpu_millicores
                .cmp(&a.allocatable().cpu_millicores)
                .then(
                    b.allocatable()
                        .memory_bytes
                        .cmp(&a.allocatable().memory_bytes),
                )
        });

        let mut job_idx = 0;
        for node in nodes.iter_mut() {
            if job_idx >= group_jobs.len() {
                break;
            }

            // How many jobs of this profile can fit on this node?
            let allocatable = node.allocatable();
            let fits = jobs_that_fit(&allocatable, required);

            let to_assign = fits.min(group_jobs.len() - job_idx);
            for _ in 0..to_assign {
                assignments.push((group_jobs[job_idx].name.clone(), node.node_id.clone()));
                // Update allocated resources so subsequent profiles see reduced capacity
                node.allocated = node.allocated.saturating_add(required);
                job_idx += 1;
            }
        }

        // Any remaining jobs in this group are unschedulable
        while job_idx < group_jobs.len() {
            unschedulable.push(group_jobs[job_idx].name.clone());
            job_idx += 1;
        }
    }

    BatchAllocation {
        assignments,
        unschedulable,
    }
}

/// Calculate how many jobs with the given resource requirement fit in the available capacity.
fn jobs_that_fit(available: &Resources, required: &Resources) -> usize {
    if required.is_zero() {
        // Zero-resource jobs (e.g. scripts with no limits) — effectively unlimited.
        // Cap at a reasonable number to prevent runaway allocation.
        return 10_000;
    }

    let by_cpu = available
        .cpu_millicores
        .checked_div(required.cpu_millicores)
        .unwrap_or(u64::MAX);

    let by_mem = available
        .memory_bytes
        .checked_div(required.memory_bytes)
        .unwrap_or(u64::MAX);

    let by_gpu = (available.gpus)
        .checked_div(required.gpus)
        .map(|v| v as u64)
        .unwrap_or(u64::MAX);

    by_cpu.min(by_mem).min(by_gpu) as usize
}

/// A resource profile key for grouping identical jobs.
///
/// Jobs with the same CPU, memory, and GPU requirements are grouped
/// together so we can bin-pack them in bulk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResourceProfile {
    cpu_millicores: u64,
    memory_bytes: u64,
    gpus: u32,
}

impl From<&Resources> for ResourceProfile {
    fn from(r: &Resources) -> Self {
        Self {
            cpu_millicores: r.cpu_millicores,
            memory_bytes: r.memory_bytes,
            gpus: r.gpus,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::SocketAddr;

    use super::*;

    fn make_node(name: &str, cpu: u64, mem: u64) -> NodeCapacity {
        NodeCapacity {
            node_id: NodeId::new(name),
            address: "127.0.0.1:9117".parse::<SocketAddr>().unwrap(),
            total: Resources::new(cpu, mem, 0),
            reserved: Resources::new(0, 0, 0),
            allocated: Resources::new(0, 0, 0),
            labels: BTreeMap::new(),
        }
    }

    fn make_job(name: &str, cpu: u64, mem: u64) -> BatchJob {
        BatchJob {
            name: name.to_string(),
            resources: Resources::new(cpu, mem, 0),
        }
    }

    #[test]
    fn schedule_empty_batch() {
        let mut nodes = vec![make_node("n1", 4000, 8_000_000_000)];
        let result = schedule_batch(&[], &mut nodes);
        assert!(result.assignments.is_empty());
        assert!(result.unschedulable.is_empty());
    }

    #[test]
    fn schedule_single_job_single_node() {
        let mut nodes = vec![make_node("n1", 4000, 8_000_000_000)];
        let jobs = vec![make_job("j1", 100, 128_000_000)];
        let result = schedule_batch(&jobs, &mut nodes);
        assert_eq!(result.assignments.len(), 1);
        assert_eq!(result.assignments[0].0, "j1");
        assert_eq!(result.assignments[0].1, NodeId::new("n1"));
        assert!(result.unschedulable.is_empty());
    }

    #[test]
    fn schedule_respects_resource_limits() {
        // Node has 1000m CPU, each job needs 300m → fits 3
        let mut nodes = vec![make_node("n1", 1000, 8_000_000_000)];
        let jobs: Vec<_> = (0..5)
            .map(|i| make_job(&format!("j{i}"), 300, 100_000_000))
            .collect();
        let result = schedule_batch(&jobs, &mut nodes);
        assert_eq!(result.assignments.len(), 3);
        assert_eq!(result.unschedulable.len(), 2);
    }

    #[test]
    fn schedule_distributes_across_nodes() {
        let mut nodes = vec![
            make_node("n1", 2000, 4_000_000_000),
            make_node("n2", 2000, 4_000_000_000),
        ];
        // 4 jobs × 1000m each → 2 per node
        let jobs: Vec<_> = (0..4)
            .map(|i| make_job(&format!("j{i}"), 1000, 1_000_000_000))
            .collect();
        let result = schedule_batch(&jobs, &mut nodes);
        assert_eq!(result.assignments.len(), 4);
        assert!(result.unschedulable.is_empty());

        // Check distribution
        let on_n1 = result
            .assignments
            .iter()
            .filter(|(_, n)| n.0 == "n1")
            .count();
        let on_n2 = result
            .assignments
            .iter()
            .filter(|(_, n)| n.0 == "n2")
            .count();
        assert_eq!(on_n1, 2);
        assert_eq!(on_n2, 2);
    }

    #[test]
    fn schedule_handles_heterogeneous_profiles() {
        let mut nodes = vec![make_node("n1", 4000, 8_000_000_000)];
        let jobs = vec![
            make_job("small", 100, 128_000_000),
            make_job("big", 2000, 4_000_000_000),
            make_job("small2", 100, 128_000_000),
        ];
        let result = schedule_batch(&jobs, &mut nodes);
        assert_eq!(result.assignments.len(), 3);
        assert!(result.unschedulable.is_empty());
    }

    #[test]
    fn schedule_handles_empty_node_list() {
        let mut nodes: Vec<NodeCapacity> = vec![];
        let jobs = vec![make_job("j1", 100, 128_000_000)];
        let result = schedule_batch(&jobs, &mut nodes);
        assert!(result.assignments.is_empty());
        assert_eq!(result.unschedulable.len(), 1);
    }

    #[test]
    fn schedule_100k_jobs_under_1_second() {
        let mut nodes: Vec<_> = (0..100)
            .map(|i| make_node(&format!("n{i}"), 100_000, 256_000_000_000))
            .collect();
        let jobs: Vec<_> = (0..100_000)
            .map(|i| make_job(&format!("j{i}"), 100, 128_000_000))
            .collect();

        let start = std::time::Instant::now();
        let result = schedule_batch(&jobs, &mut nodes);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs_f64() < 1.0,
            "batch scheduling took {elapsed:.2?}, expected <1s"
        );
        assert_eq!(result.assignments.len(), 100_000);
        assert!(result.unschedulable.is_empty());
    }

    #[test]
    fn batch_id_display() {
        assert_eq!(BatchId(42).to_string(), "batch-42");
    }

    #[test]
    fn jobs_that_fit_zero_resources() {
        let available = Resources::new(4000, 8_000_000_000, 0);
        let required = Resources::new(0, 0, 0);
        assert_eq!(jobs_that_fit(&available, &required), 10_000);
    }

    #[test]
    fn jobs_that_fit_exact() {
        let available = Resources::new(1000, 1_000_000_000, 0);
        let required = Resources::new(500, 500_000_000, 0);
        assert_eq!(jobs_that_fit(&available, &required), 2);
    }

    #[test]
    fn jobs_that_fit_memory_limited() {
        let available = Resources::new(10_000, 1_000_000_000, 0);
        let required = Resources::new(100, 500_000_000, 0);
        // CPU: 10000/100 = 100, Mem: 1000000000/500000000 = 2 → limited by memory
        assert_eq!(jobs_that_fit(&available, &required), 2);
    }
}
