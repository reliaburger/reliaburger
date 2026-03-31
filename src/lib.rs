//! Reliaburger core library.
//!
//! Contains all subsystems for the Reliaburger container orchestrator:
//! container runtime (Grill), health checking, scheduling (Meat),
//! gossip protocol (Mustard), and everything else.
//!
//! Modules are added incrementally as each roadmap phase is implemented.

pub mod bun;
pub mod config;
pub mod council;
pub mod firewall;
pub mod grill;
pub mod meat;
pub mod mustard;
pub mod onion;
pub mod reconstruction;
pub mod relish;
pub mod reporting;
pub mod sesame;
pub mod wrapper;
