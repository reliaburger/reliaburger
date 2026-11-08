//! Agent changes as proposals you can undo, and the leases that keep two
//! changers apart (0.2.8, #1682 and #1686).
//!
//! The design is `docs/plans/2026-11-08-design-agent-proposals.md`. This
//! module grows in the order that design gives: [`lease`] first, then the
//! proposals that take leases before they apply.

pub mod lease;
