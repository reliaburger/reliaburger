//! Agent changes as proposals you can undo, and the leases that keep two
//! changers apart (0.2.8, #1682 and #1686).
//!
//! The design is `docs/plans/2026-11-08-design-agent-proposals.md`. An
//! agent never changes the cluster: it proposes. A proposal holds each
//! touched app before and after ([`types`]), the words and numbers a person
//! reads before approving ([`build`]), and moves through the states in
//! [`machine`]. [`revision`] notices that someone else changed an app in
//! between, [`lease`] keeps two changers apart, and [`guard`] is where the
//! policies judge a proposal.

pub mod build;
pub mod guard;
pub mod lease;
pub mod machine;
pub mod revision;
pub mod types;
pub mod view;

pub use build::{ProposalRequest, RequestKind};
pub use machine::{ApplyBlock, Proposals};
pub use types::*;
