//! Reliaburger core library.
//!
//! Contains all subsystems for the Reliaburger container orchestrator:
//! container runtime (Grill), health checking, scheduling (Patty),
//! gossip protocol (Mustard), and everything else.
//!
//! Modules are added incrementally as each roadmap phase is implemented.

pub mod config;
pub mod grill;
