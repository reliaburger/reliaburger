//! Mayo — time-series metrics database.
//!
//! Built on Arrow RecordBatches for in-memory storage, DataFusion for
//! SQL queries, Parquet for persistence, and object_store for
//! local/S3/GCS storage abstraction. The same architecture as
//! InfluxDB IOx.

pub mod alert;
pub mod collector;
pub mod query_fanout;
pub mod rollup;
pub mod rollup_generator;
pub mod rollup_store;
pub mod rollup_worker;
pub mod scrape;
pub mod store;
pub mod types;
