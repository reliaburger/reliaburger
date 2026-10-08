//! Bounded image-compatible command slots beneath the common job lifecycle.
#[cfg(target_os = "linux")]
mod pool;
#[cfg(target_os = "linux")]
pub(crate) use pool::ReusablePool;
#[cfg(any(target_os = "linux", test))]
mod protocol;
use crate::config::job::JobSpec;
use crate::config::types::ResourceRange;
use crate::meat::Resources;

/// Explicit reservation for the container's idle command helper.
pub const HELPER_CPU_REQUEST: u64 = 10;
/// The helper is independently capped, outside each command's memory limit.
pub const HELPER_MEMORY_BYTES: u64 = 8 << 20;

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExecutorKey([u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutorProfile {
    pub cpu: ResourceRange,
    pub memory: ResourceRange,
    pub reservation: Resources,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecutorError {
    #[error("reusable-container refused: {0}")]
    Configuration(&'static str),
    #[cfg(any(target_os = "linux", test))]
    #[error("cannot encode executor configuration: {0}")]
    Encoding(#[from] serde_json::Error),
    #[cfg(any(target_os = "linux", test))]
    #[error("executor I/O: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(any(target_os = "linux", test))]
    #[error("executor runtime: {0}")]
    Runtime(#[from] crate::grill::GrillError),
    #[cfg(any(target_os = "linux", test))]
    #[error("executor protocol: {0}")]
    Protocol(String),
}

#[cfg(any(target_os = "linux", test))]
impl ExecutorKey {
    pub(crate) fn new(template: &JobSpec) -> Result<Self, ExecutorError> {
        use sha2::{Digest, Sha256};
        if template.exec.is_some() || template.script.is_some() {
            return Err(ExecutorError::Configuration(
                "reusable-container is image-only",
            ));
        }
        let image = template
            .image
            .as_deref()
            .ok_or(ExecutorError::Configuration("an image is required"))?;
        let reference = crate::grill::image::ImageReference::parse(image)
            .map_err(|_| ExecutorError::Configuration("invalid image reference"))?;
        if !reference.tag.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ExecutorError::Configuration(
                "the admitted image must be pinned by digest",
            ));
        }
        if template.env.values().any(|value| value.is_encrypted()) {
            return Err(ExecutorError::Configuration(
                "credentials must be resolved against live namespace keys",
            ));
        }
        let profile = ExecutorProfile::new(template)?;
        let mut compatible = template.clone();
        compatible.image = Some(reference.full_reference());
        compatible.namespace = Some(template.namespace.as_deref().unwrap_or("default").into());
        compatible.command = None;
        compatible.schedule = None;
        compatible.run_before.clear();
        compatible.script = None;
        compatible.cpu = Some(profile.cpu);
        compatible.memory = Some(profile.memory);
        let encoded = serde_json::to_vec(&compatible)?;
        Ok(Self(Sha256::digest(encoded).into()))
    }
}
impl ExecutorProfile {
    pub(crate) fn new(template: &JobSpec) -> Result<Self, ExecutorError> {
        let cpu = template.cpu.unwrap_or(ResourceRange {
            request: 1000,
            limit: 1000,
        });
        let memory = template.memory.unwrap_or(ResourceRange {
            request: 64 << 20,
            limit: 64 << 20,
        });
        if cpu.request == 0
            || cpu.limit < cpu.request
            || cpu.limit.checked_mul(100_000).is_none()
            || memory.request == 0
            || memory.limit < memory.request
            || memory.limit > i64::MAX as u64
        {
            return Err(ExecutorError::Configuration(
                "unsupported CPU or memory range",
            ));
        }
        let reservation = Resources::new(
            cpu.request
                .checked_add(HELPER_CPU_REQUEST)
                .ok_or(ExecutorError::Configuration("CPU reservation overflow"))?,
            memory
                .request
                .checked_add(HELPER_MEMORY_BYTES)
                .ok_or(ExecutorError::Configuration("memory reservation overflow"))?,
            0,
        );
        Ok(Self {
            cpu,
            memory,
            reservation,
        })
    }
}

/// A bounded live tail for one command, never the next occupant of its slot.
pub(crate) struct CommandLogStream {
    state: std::sync::Mutex<(
        std::collections::VecDeque<crate::ketchup::types::CapturedLine>,
        usize,
    )>,
    changed: tokio::sync::broadcast::Sender<crate::ketchup::types::CapturedLine>,
}
impl CommandLogStream {
    #[cfg(target_os = "linux")]
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: Default::default(),
            changed: tokio::sync::broadcast::channel(256).0,
        })
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn push(&self, line: crate::ketchup::types::CapturedLine) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 += line.line.len();
        state.0.push_back(line.clone());
        while state.0.len() > 256 || state.1 > 65_536 {
            if let Some(old) = state.0.pop_front() {
                state.1 -= old.line.len();
            }
        }
        let _ = self.changed.send(line);
    }
    pub(crate) async fn follow(
        &self,
        output: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
        retired: &tokio_util::sync::CancellationToken,
    ) {
        let (history, mut live) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                state.0.iter().cloned().collect::<Vec<_>>(),
                self.changed.subscribe(),
            )
        };
        for line in history {
            if output.send(line).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! { biased;
                () = retired.cancelled() => return,
                line = live.recv() => match line {
                    Ok(line) => if output.send(line).await.is_err() { return; },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = output.send(crate::ketchup::types::CapturedLine { stream: crate::ketchup::types::LogStream::Stderr,
                            line: "Live log reader fell behind; query the stored run logs for the missing lines".into(), position: None }).await;
                    },
                    Err(_) => return,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn template() -> JobSpec {
        toml::from_str("image='fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nnamespace='tenant-a'\nisolation='reusable-container'\ncpu='100m-1000m'\nmemory='32Mi-64Mi'\n[env]\nTOKEN='first'").unwrap()
    }
    #[test]
    fn compatible_commands_share_a_slot_but_trust_and_profiles_do_not() {
        let first = template();
        let key = ExecutorKey::new(&first).unwrap();
        let mut next = first.clone();
        next.command = Some(vec!["other-command".into()]);
        next.schedule = Some("0 * * * *".into());
        next.run_before = vec!["app.web".into()];
        assert_eq!(ExecutorKey::new(&next).unwrap(), key);
        next.namespace = Some("tenant-b".into());
        assert_ne!(ExecutorKey::new(&next).unwrap(), key);
        next = first.clone();
        next.env.insert(
            "TOKEN".into(),
            crate::config::types::EnvValue::Plain("rotated".into()),
        );
        assert_ne!(ExecutorKey::new(&next).unwrap(), key);
        next = first.clone();
        next.memory.as_mut().unwrap().limit += 1;
        assert_ne!(ExecutorKey::new(&next).unwrap(), key);
        next = first;
        next.image = Some(
            "fixture@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
        );
        assert_ne!(ExecutorKey::new(&next).unwrap(), key);
    }
    #[test]
    fn pools_require_resolved_credentials_and_pinned_images() {
        let mut job = template();
        job.image = Some("fixture:latest".into());
        assert!(ExecutorKey::new(&job).is_err());
        job = template();
        job.env.insert(
            "TOKEN".into(),
            crate::config::types::EnvValue::Encrypted("sealed".into()),
        );
        assert!(ExecutorKey::new(&job).is_err());
    }
    #[test]
    fn idle_profile_reservation_includes_the_helper_without_weakening_task_limits() {
        let profile = ExecutorProfile::new(&template()).unwrap();
        assert_eq!(profile.reservation, Resources::new(110, 40 << 20, 0));
        assert_eq!(profile.cpu.limit, 1000);
        assert_eq!(profile.memory.limit, 64 << 20);
        let mut invalid = template();
        invalid.cpu.as_mut().unwrap().request = 0;
        assert!(ExecutorProfile::new(&invalid).is_err());
        invalid = template();
        invalid.memory.as_mut().unwrap().limit = 0;
        assert!(ExecutorProfile::new(&invalid).is_err());
    }
}
