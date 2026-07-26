//! The chaos scenarios.
//!
//! Scenarios land in step 14, once stream C has made node-level faults
//! actually work — three of the five drive `node-kill`, which today refuses.

use super::registry::TestCase;

/// The chaos scenarios, selected by `--chaos`.
pub fn scenarios() -> Vec<TestCase> {
    Vec::new()
}
