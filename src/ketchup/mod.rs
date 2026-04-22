//! Ketchup — log collection and querying.
//!
//! Captures stdout/stderr from workloads into append-only files
//! with sparse timestamp indexes for efficient time-range queries.

pub mod export;
pub mod index;
pub mod json;
pub mod log_store;
pub mod query;
pub mod remote_query;
pub mod store;
pub mod types;
