//! The built-in cluster test and benchmark harness.
//!
//! Everything here runs **client-side**, inside the `relish` process, and
//! talks to the cluster over `BunClient` exactly as an operator would. That
//! is deliberate: a test that reaches inside the agent proves the agent
//! agrees with itself, which is not the question. These cases prove the
//! cluster does what its public API promises.
//!
//! - [`report`] — the `--output json` contract.
//! - [`registry`] — the catalogue and `--filter` selection.
//! - [`context`] — what a case is handed, and namespace isolation.
//! - [`runner`] — parallelism, timeouts and teardown around a selection.
//! - [`cases`] — the integration catalogue.
//! - [`chaos`] — the chaos scenarios, selected by `--chaos`.

pub mod cases;
pub mod chaos;
pub mod context;
pub mod deadline;
pub mod oci;
pub mod registry;
pub mod report;
pub mod runner;
pub mod safety;

pub use context::TestContext;
pub use registry::{TestCase, TestFn, all_cases, chaos_cases, parse_filter, select, unknown};
pub use report::{
    CleanupOutcome, EvidenceKind, TestCaseResult, TestEvidence, TestGroup, TestOutcome,
    TestProfile, TestReport, UnknownKind,
};
pub use runner::{RunConfig, run};
