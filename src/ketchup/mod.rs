//! Ketchup — log collection and querying.
//!
//! Captures stdout/stderr from workloads into append-only files
//! with sparse timestamp indexes for efficient time-range queries.

pub mod export;
pub mod log_store;
pub mod query;
pub mod remote_query;
pub mod types;
