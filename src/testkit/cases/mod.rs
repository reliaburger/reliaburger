//! The integration catalogue.
//!
//! Cases land here in steps 7 and 8 of the plan. The registry already knows
//! how to select and run them, so an empty catalogue is a valid (if dull)
//! run rather than a compile error.

use super::registry::TestCase;

/// Every integration case, in the order groups are reported.
pub fn all() -> Vec<TestCase> {
    Vec::new()
}
