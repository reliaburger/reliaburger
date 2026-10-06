//! Pickle — Reliaburger's built-in OCI image registry.
//!
//! Provides content-addressed blob storage, the OCI Distribution API
//! for push/pull, synchronous replication, pull-through caching,
//! and garbage collection with sole-copy protection.

pub mod api;
pub mod authority;
pub mod binding;
pub mod build;
pub mod capability;
pub mod copy;
pub mod cosign;
pub mod gc;
pub mod lease;
pub mod p2p;
pub mod pull;
pub mod registry_auth;
pub mod replication;
pub mod signing;
mod storage_budget;
pub mod store;
pub mod trust;
pub mod types;
pub mod upstream;
