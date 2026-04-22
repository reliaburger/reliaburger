//! Brioche — embedded web dashboard.
//!
//! A multi-page cluster dashboard served as HTML from the Bun agent.
//! Uses HTMX for partial-page refreshes and uPlot for time-series
//! charts. Static assets (JS, CSS) are compiled into the binary via
//! `rust-embed`.

pub mod app_detail;
pub mod assets;
pub mod dashboard;
pub mod fragments;
pub mod node_detail;
pub mod types;
