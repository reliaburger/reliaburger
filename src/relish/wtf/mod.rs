//! Deterministic cluster diagnosis for `relish wtf`.
//!
//! Collection is deliberately separate from diagnosis. The collector records
//! what it observed, including unavailable and unsupported sources, while the
//! pure engine turns that evidence into a stable report.

mod collect;
mod diagnose;
mod model;

pub use collect::collect;
pub(crate) use collect::node_client;
pub use diagnose::diagnose;
pub use model::{
    AlertObservation, ApplicationEvidence, CertificateObservation, ClusterEvidence,
    CorrelatedEvent, CouncilObservation, CpuThrottleObservation, DeployObservation,
    DiskObservation, Evidence, FaultObservation, LogObservation, NodeObservation,
    RegistryObservation, RestartObservation, ServiceObservation, WtfFinding, WtfInputs, WtfOk,
    WtfReport, WtfSummary, WtfUnknown,
};

/// Current serialised `wtf` report contract.
pub const WTF_SCHEMA_VERSION: u32 = 1;
