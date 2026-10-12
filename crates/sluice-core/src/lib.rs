//! Pure-Rust logic for pg_sluice: tag parsing, rule schema, rule matching, token bucket, EMA.
//! Shared by the extension and the CLI; tests run without Postgres.

pub mod tags;

pub use tags::{Tags, parse_tags};
