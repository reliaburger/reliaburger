//! Ketchup — log collection and querying.
//!
//! Captures stdout/stderr from workloads into append-only files
//! with sparse timestamp indexes for efficient time-range queries.

pub mod index;
pub mod json;
pub mod store;
pub mod types;
