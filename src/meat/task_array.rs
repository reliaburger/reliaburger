//! Task arrays: one job template, many indexed tasks.
//!
//! A task array is how a million jobs become one small request. The
//! submission carries a single [`JobSpec`] template plus a
//! [`TaskArraySpec`] (the count and the policy); every index in
//! `0..count` becomes one process on some node, with `{index}` in its
//! arguments replaced by the task's own index.
//!
//! Indices are grouped into fixed-size *chunks*. The chunk, not the task,
//! is what the leader allocates to nodes and records in Raft, so the
//! control plane's cost grows with `count / chunk_size`; at a fixed
//! chunk size it still grows with the task count. See `docs/plans/2026-09-28-plan-million-jobs.md`.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::config::job::JobSpec;

/// Largest task count one array may have (2^24, about 16.7 million).
pub const MAX_TASK_COUNT: u32 = 1 << 24;

/// Largest chunk: 65,536 tasks.
pub const MAX_CHUNK_SIZE: u32 = 1 << 16;

/// Most chunks one array may have. Bounds the leader's per-array state
/// even for a pathological `chunk_size = 1`.
pub const MAX_CHUNKS: u32 = 1 << 16;

/// Default chunk size: a million tasks become 977 chunks.
pub const DEFAULT_CHUNK_SIZE: u32 = 1024;

/// Most attempts a task may be given.
pub const MAX_ATTEMPTS: u8 = 10;

/// Default attempts per task (the first run plus two retries).
pub const DEFAULT_MAX_ATTEMPTS: u8 = 3;

/// Longest per-task timeout: one day.
pub const MAX_TASK_TIMEOUT_SECS: u32 = 86_400;

/// Default per-task timeout: ten minutes.
pub const DEFAULT_TASK_TIMEOUT_SECS: u32 = 600;

/// The placeholder replaced by the task's index in every argument.
pub const INDEX_PLACEHOLDER: &str = "{index}";

/// Identifies one chunk within an array: chunk `c` holds indices
/// `c * chunk_size ..= min((c + 1) * chunk_size, count) - 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChunkId(pub u32);

/// Count and policy for a task array. Travels beside the job template
/// rather than inside it, keeping ordinary job configuration separate from
/// delegated execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArraySpec {
    /// Number of tasks; indices run from 0 to `count - 1`.
    #[serde(default = "default_count")]
    pub count: u32,
    /// Tasks per chunk.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: u32,
    /// Attempts per task before it counts as failed.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    /// Stop the whole array once more than this many indices have failed
    /// for good. `None` never stops early.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failed_indexes: Option<u32>,
    /// Per-attempt wall-clock limit; zero disables the deadline.
    #[serde(default = "default_task_timeout_secs")]
    pub task_timeout_secs: u32,
    /// Most tasks of this array one node runs at once. `None` lets each
    /// node use its configured safety cap, further bounded by resource requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_node_concurrency: Option<u32>,
}

fn default_count() -> u32 {
    1
}

fn default_chunk_size() -> u32 {
    DEFAULT_CHUNK_SIZE
}

fn default_max_attempts() -> u8 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_task_timeout_secs() -> u32 {
    DEFAULT_TASK_TIMEOUT_SECS
}

/// Why a task array submission was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskArraySpecError {
    #[error("task count must be between 1 and {MAX_TASK_COUNT}, got {count}")]
    CountOutOfRange { count: u32 },
    #[error("chunk size must be between 1 and {MAX_CHUNK_SIZE}, got {chunk_size}")]
    ChunkSizeOutOfRange { chunk_size: u32 },
    #[error(
        "{count} tasks in chunks of {chunk_size} is {chunks} chunks; the limit is {MAX_CHUNKS}"
    )]
    TooManyChunks {
        count: u32,
        chunk_size: u32,
        chunks: u32,
    },
    #[error("max attempts must be between 1 and {MAX_ATTEMPTS}, got {max_attempts}")]
    AttemptsOutOfRange { max_attempts: u8 },
    #[error("task timeout must be between 1 and {MAX_TASK_TIMEOUT_SECS} seconds, got {seconds}")]
    TimeoutOutOfRange { seconds: u32 },
    #[error("per-node concurrency must be at least 1")]
    ZeroConcurrency,
    #[error(
        "a task array template needs exactly one of `image` or `exec`; {found} isn't supported"
    )]
    UnsupportedTemplate { found: &'static str },
    #[error("a task array template can't have {field}")]
    TemplateField { field: &'static str },
}

impl TaskArraySpec {
    /// A spec with `count` tasks and every default.
    pub fn with_count(count: u32) -> Self {
        Self {
            count,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_failed_indexes: None,
            task_timeout_secs: DEFAULT_TASK_TIMEOUT_SECS,
            per_node_concurrency: None,
        }
    }

    /// Check every limit. Chunk maths below assumes a validated spec.
    pub fn validate(&self) -> Result<(), TaskArraySpecError> {
        if self.count == 0 || self.count > MAX_TASK_COUNT {
            return Err(TaskArraySpecError::CountOutOfRange { count: self.count });
        }
        if self.chunk_size == 0 || self.chunk_size > MAX_CHUNK_SIZE {
            return Err(TaskArraySpecError::ChunkSizeOutOfRange {
                chunk_size: self.chunk_size,
            });
        }
        let chunks = self.chunk_count();
        if chunks > MAX_CHUNKS {
            return Err(TaskArraySpecError::TooManyChunks {
                count: self.count,
                chunk_size: self.chunk_size,
                chunks,
            });
        }
        if self.max_attempts == 0 || self.max_attempts > MAX_ATTEMPTS {
            return Err(TaskArraySpecError::AttemptsOutOfRange {
                max_attempts: self.max_attempts,
            });
        }
        if self.task_timeout_secs > MAX_TASK_TIMEOUT_SECS {
            return Err(TaskArraySpecError::TimeoutOutOfRange {
                seconds: self.task_timeout_secs,
            });
        }
        if self.per_node_concurrency == Some(0) {
            return Err(TaskArraySpecError::ZeroConcurrency);
        }
        Ok(())
    }

    /// Number of chunks, rounding up. Zero only for an invalid spec.
    pub fn chunk_count(&self) -> u32 {
        self.count.div_ceil(self.chunk_size.max(1))
    }

    /// The indices chunk `chunk` holds; the last chunk may be short.
    /// `None` for a chunk past the end.
    pub fn chunk_range(&self, chunk: ChunkId) -> Option<RangeInclusive<u32>> {
        if chunk.0 >= self.chunk_count() {
            return None;
        }
        // chunk < chunk_count, so first < count <= 2^24: no overflow.
        let first = chunk.0 * self.chunk_size;
        let last = first
            .saturating_add(self.chunk_size - 1)
            .min(self.count - 1);
        Some(first..=last)
    }

    /// The chunk that holds `index`, or `None` past the end.
    pub fn chunk_of(&self, index: u32) -> Option<ChunkId> {
        if index >= self.count || self.chunk_size == 0 {
            return None;
        }
        Some(ChunkId(index / self.chunk_size))
    }
}

/// Check a homogeneous image or host template. Cron schedules, inline scripts
/// and dependency hooks do not describe individual delegated tasks.
pub fn validate_template(template: &JobSpec) -> Result<(), TaskArraySpecError> {
    if template.isolation == crate::config::job::ContainerIsolation::ReusableContainer
        && template.image.is_none()
    {
        return Err(TaskArraySpecError::TemplateField {
            field: "reusable-container without an image",
        });
    }
    // At most 64 active profiles are copied into a node sync. Bound each
    // template so argv/environment cannot turn that into an unbounded RPC.
    if serde_json::to_vec(template).map_or(true, |bytes| bytes.len() > 16 * 1024) {
        return Err(TaskArraySpecError::TemplateField {
            field: "a template larger than 16 KiB",
        });
    }
    if usize::from(template.image.is_some())
        + usize::from(template.exec.is_some())
        + usize::from(template.script.is_some())
        != 1
    {
        return Err(TaskArraySpecError::UnsupportedTemplate {
            found: "a template without exactly one of image, exec or script",
        });
    }
    if template.cpu.is_some_and(|r| r.request == 0)
        || template.memory.is_some_and(|r| r.request == 0)
    {
        return Err(TaskArraySpecError::TemplateField {
            field: "a zero CPU or memory request",
        });
    }
    if template.schedule.is_some() {
        return Err(TaskArraySpecError::TemplateField {
            field: "a cron schedule",
        });
    }
    if !template.run_before.is_empty() {
        return Err(TaskArraySpecError::TemplateField {
            field: "run_before dependencies",
        });
    }
    Ok(())
}

/// The task's arguments: every `{index}` in every argument replaced by
/// `index`. There's no other template language on purpose; a task that
/// needs more derives it from its index.
pub fn expand_argv(template: &[String], index: u32) -> Vec<String> {
    let rendered = index.to_string();
    template
        .iter()
        .map(|argument| argument.replace(INDEX_PLACEHOLDER, &rendered))
        .collect()
}

/// The environment variables every task gets, beside the template's own.
pub fn task_env(batch_id: u64, count: u32, index: u32, attempt: u8) -> [(&'static str, String); 4] {
    [
        ("RELIABURGER_BATCH_ID", batch_id.to_string()),
        ("RELIABURGER_TASK_COUNT", count.to_string()),
        ("RELIABURGER_TASK_INDEX", index.to_string()),
        ("RELIABURGER_TASK_ATTEMPT", attempt.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;

    fn exec_template(args: &[&str]) -> JobSpec {
        JobSpec {
            isolation: Default::default(),
            image: None,
            command: Some(args.iter().map(|a| a.to_string()).collect()),
            schedule: None,
            run_before: Vec::new(),
            memory: None,
            cpu: None,
            env: Default::default(),
            namespace: None,
            exec: Some(PathBuf::from("/usr/local/bin/rb-task")),
            script: None,
        }
    }

    #[test]
    fn a_zero_attempt_timeout_means_no_deadline_for_long_running_work() {
        let mut spec = TaskArraySpec::with_count(1);
        spec.task_timeout_secs = 0;
        spec.validate().unwrap();
    }

    #[test]
    fn common_templates_accept_authorised_scripts_and_encrypted_environment() {
        let mut job = exec_template(&[]);
        job.exec = None;
        job.script = Some("echo {index}".into());
        job.env.insert(
            "TOKEN".into(),
            crate::config::types::EnvValue::Encrypted("ciphertext".into()),
        );
        assert_eq!(validate_template(&job), Ok(()));
        job.exec = Some("/bin/true".into());
        assert!(validate_template(&job).is_err());
    }

    #[test]
    fn omitted_task_count_means_one() {
        let spec: TaskArraySpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec, TaskArraySpec::with_count(1));
    }

    #[test]
    fn defaults_are_valid() {
        let spec = TaskArraySpec::with_count(1_000_000);
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(spec.chunk_count(), 977);
    }

    #[test]
    fn limits_are_accepted_at_the_edges() {
        let mut spec = TaskArraySpec::with_count(MAX_TASK_COUNT);
        spec.chunk_size = MAX_TASK_COUNT / MAX_CHUNKS;
        assert_eq!(spec.validate(), Ok(()));
        let mut one = TaskArraySpec::with_count(1);
        one.chunk_size = 1;
        one.max_attempts = MAX_ATTEMPTS;
        one.task_timeout_secs = MAX_TASK_TIMEOUT_SECS;
        assert_eq!(one.validate(), Ok(()));
    }

    #[test]
    fn out_of_range_specs_are_refused() {
        let base = TaskArraySpec::with_count(100);
        let cases = [
            (
                TaskArraySpec {
                    count: 0,
                    ..base.clone()
                },
                TaskArraySpecError::CountOutOfRange { count: 0 },
            ),
            (
                TaskArraySpec {
                    count: MAX_TASK_COUNT + 1,
                    ..base.clone()
                },
                TaskArraySpecError::CountOutOfRange {
                    count: MAX_TASK_COUNT + 1,
                },
            ),
            (
                TaskArraySpec {
                    chunk_size: 0,
                    ..base.clone()
                },
                TaskArraySpecError::ChunkSizeOutOfRange { chunk_size: 0 },
            ),
            (
                TaskArraySpec {
                    chunk_size: MAX_CHUNK_SIZE + 1,
                    ..base.clone()
                },
                TaskArraySpecError::ChunkSizeOutOfRange {
                    chunk_size: MAX_CHUNK_SIZE + 1,
                },
            ),
            (
                TaskArraySpec {
                    count: MAX_CHUNKS + 1,
                    chunk_size: 1,
                    ..base.clone()
                },
                TaskArraySpecError::TooManyChunks {
                    count: MAX_CHUNKS + 1,
                    chunk_size: 1,
                    chunks: MAX_CHUNKS + 1,
                },
            ),
            (
                TaskArraySpec {
                    max_attempts: 0,
                    ..base.clone()
                },
                TaskArraySpecError::AttemptsOutOfRange { max_attempts: 0 },
            ),
            (
                TaskArraySpec {
                    max_attempts: MAX_ATTEMPTS + 1,
                    ..base.clone()
                },
                TaskArraySpecError::AttemptsOutOfRange {
                    max_attempts: MAX_ATTEMPTS + 1,
                },
            ),
            (
                TaskArraySpec {
                    task_timeout_secs: MAX_TASK_TIMEOUT_SECS + 1,
                    ..base.clone()
                },
                TaskArraySpecError::TimeoutOutOfRange {
                    seconds: MAX_TASK_TIMEOUT_SECS + 1,
                },
            ),
            (
                TaskArraySpec {
                    per_node_concurrency: Some(0),
                    ..base.clone()
                },
                TaskArraySpecError::ZeroConcurrency,
            ),
        ];
        for (spec, expected) in cases {
            assert_eq!(spec.validate(), Err(expected));
        }
    }

    #[test]
    fn last_chunk_is_short() {
        let spec = TaskArraySpec {
            chunk_size: 1024,
            ..TaskArraySpec::with_count(1_000_000)
        };
        assert_eq!(spec.chunk_range(ChunkId(0)), Some(0..=1023));
        assert_eq!(spec.chunk_range(ChunkId(976)), Some(999_424..=999_999));
        assert_eq!(spec.chunk_range(ChunkId(977)), None);
    }

    #[test]
    fn chunk_of_inverts_chunk_range() {
        let spec = TaskArraySpec {
            chunk_size: 7,
            ..TaskArraySpec::with_count(100)
        };
        assert_eq!(spec.chunk_of(0), Some(ChunkId(0)));
        assert_eq!(spec.chunk_of(6), Some(ChunkId(0)));
        assert_eq!(spec.chunk_of(7), Some(ChunkId(1)));
        assert_eq!(spec.chunk_of(99), Some(ChunkId(14)));
        assert_eq!(spec.chunk_of(100), None);
    }

    #[test]
    fn spec_json_fills_defaults() {
        let spec: TaskArraySpec = serde_json::from_str(r#"{"count": 5}"#).unwrap();
        assert_eq!(spec, TaskArraySpec::with_count(5));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(
            !json.contains("max_failed_indexes"),
            "absent options aren't written: {json}"
        );
    }

    #[test]
    fn index_placeholder_is_replaced_everywhere() {
        let template: Vec<String> = [
            "square",
            "{index}",
            "--out=/tmp/{index}-{index}.txt",
            "plain",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            expand_argv(&template, 42),
            vec!["square", "42", "--out=/tmp/42-42.txt", "plain"]
        );
        assert_eq!(expand_argv(&[], 7), Vec::<String>::new());
    }

    #[test]
    fn every_task_gets_its_identity_in_the_environment() {
        let env = task_env(7, 1_000_000, 424_242, 2);
        assert_eq!(
            env,
            [
                ("RELIABURGER_BATCH_ID", "7".to_string()),
                ("RELIABURGER_TASK_COUNT", "1000000".to_string()),
                ("RELIABURGER_TASK_INDEX", "424242".to_string()),
                ("RELIABURGER_TASK_ATTEMPT", "2".to_string()),
            ]
        );
    }

    #[test]
    fn templates_accept_images_and_exec_but_refuse_dependencies() {
        assert_eq!(validate_template(&exec_template(&["{index}"])), Ok(()));

        let mut image = exec_template(&[]);
        image.exec = None;
        image.image = Some("alpine:3".to_string());
        assert_eq!(validate_template(&image), Ok(()));
        image.exec = Some("/bin/true".into());
        assert!(validate_template(&image).is_err());

        let mut scheduled = exec_template(&[]);
        scheduled.schedule = Some("* * * * *".to_string());
        assert!(matches!(
            validate_template(&scheduled),
            Err(TaskArraySpecError::TemplateField { .. })
        ));

        let mut ordered = exec_template(&[]);
        ordered.run_before = vec!["app.web".to_string()];
        assert!(matches!(
            validate_template(&ordered),
            Err(TaskArraySpecError::TemplateField { .. })
        ));
    }

    #[test]
    fn a_million_task_submission_is_a_few_hundred_bytes() {
        let template = exec_template(&["square", "{index}"]);
        let spec = TaskArraySpec::with_count(1_000_000);
        let body = serde_json::to_vec(&(template, spec)).unwrap();
        assert!(body.len() < 4096, "submission is {} bytes", body.len());
    }

    proptest! {
        #[test]
        fn chunks_partition_the_index_space(count in 1u32..50_000, chunk_size in 1u32..5_000) {
            let spec = TaskArraySpec { chunk_size, ..TaskArraySpec::with_count(count) };
            prop_assume!(spec.validate().is_ok());
            let mut next = 0u32;
            for chunk in 0..spec.chunk_count() {
                let range = spec.chunk_range(ChunkId(chunk)).unwrap();
                prop_assert_eq!(*range.start(), next);
                prop_assert!(range.end() - range.start() < chunk_size);
                prop_assert_eq!(spec.chunk_of(*range.start()), Some(ChunkId(chunk)));
                prop_assert_eq!(spec.chunk_of(*range.end()), Some(ChunkId(chunk)));
                next = range.end() + 1;
            }
            prop_assert_eq!(next, count);
        }
    }
}
