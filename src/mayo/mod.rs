//! Mayo — time-series metrics database.
//!
//! Built on Arrow RecordBatches for in-memory storage, DataFusion for
//! SQL queries, Parquet for persistence, and object_store for
//! local/S3/GCS storage abstraction. The same architecture as
//! InfluxDB IOx.

pub mod store;
pub mod types;
