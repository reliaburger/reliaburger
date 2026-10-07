//! Optimized boundary tests compile the actual production cron parser.
//! The module path shares production source rather than copying its implementation.
#[path = "../../../src/meat/cron.rs"]
pub mod cron;
