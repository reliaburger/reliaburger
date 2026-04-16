//! Pickle — Reliaburger's built-in OCI image registry.
//!
//! Provides content-addressed blob storage, the OCI Distribution API
//! for push/pull, synchronous replication, pull-through caching,
//! and garbage collection with sole-copy protection.

pub mod api;
pub mod build;
pub mod gc;
pub mod pull;
pub mod replication;
pub mod signing;
pub mod store;
pub mod types;
