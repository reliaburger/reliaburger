//! Brioche — embedded web dashboard.
//!
//! A minimal cluster overview page served as static HTML from the
//! Bun agent. No JavaScript frameworks, no build pipeline. Data is
//! rendered server-side and refreshed via `<meta http-equiv="refresh">`.

pub mod dashboard;
