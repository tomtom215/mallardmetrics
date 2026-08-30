pub mod api;
pub mod config;
pub mod dashboard;
pub mod ingest;
pub mod query;
pub mod server;
pub mod storage;

/// Test fixtures shared by unit and integration tests.
///
/// Compiled for the crate's own tests, and for integration tests through the
/// `testing` feature. Never part of a release build.
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
