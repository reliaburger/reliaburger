//! Running Reliaburger as the appliance OS: everything bun does before the
//! agent starts on a machine that has only been told who it is.
//!
//! `bun appliance prepare` (run by `reliaburger-seed.service` before the
//! agent) finds the node's seed, and either installs the bootstrap material
//! of the node that creates the cluster or joins the cluster in-process:
//! enrol with the join token, then fetch the master key with the new
//! certificate. It writes `node.toml` last, so a node is either fully
//! prepared or not at all, and each step that already happened is skipped
//! on the next boot. See docs/plans/2026-10-01-plan-appliance-product.md, W2.

pub mod address;
pub mod console;
pub mod prepare;
pub mod seed;

pub use prepare::{Paths, PrepareError, Step, next_step};
pub use seed::{Seed, SeedConfig, SeedError, SeedRole};
