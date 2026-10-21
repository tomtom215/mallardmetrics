//! The optional extras script, served at `/mallard-extras.js`.
//!
//! Same pattern as the main tracker (`dashboard::serve_tracking_script`): one
//! source file compiled in with `include_str!`, a one-hour cache, and CORS open
//! to every origin so any site can embed it. It has its own 2,048-byte gzip
//! budget, enforced by a test below and by CI.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

/// The extras script, compiled into the binary from its single source.
pub const EXTRAS_SCRIPT: &str = include_str!("../../tracking/extras.js");

/// Serve the extras script.
pub async fn serve_extras_script() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (
                header::CACHE_CONTROL,
                crate::dashboard::SCRIPT_CACHE_CONTROL,
            ),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        EXTRAS_SCRIPT,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Gzipped budget in bytes (`gzip -9`), the same measure CI uses.
    const BUDGET: usize = 2048;

    #[test]
    fn test_extras_script_stays_inside_its_gzip_budget() {
        let required =
            std::env::var_os("MALLARD_REQUIRE_NODE").is_some() || std::env::var_os("CI").is_some();
        let spawned = Command::new("gzip")
            .arg("-9")
            .arg("-c")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if !required => {
                eprintln!("skipping the extras size budget: gzip is not available ({e})");
                return;
            }
            Err(e) => panic!("gzip is required for the size budget: {e}"),
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(EXTRAS_SCRIPT.as_bytes())
            .unwrap();
        let size = child.wait_with_output().unwrap().stdout.len();
        assert!(
            size <= BUDGET,
            "tracking/extras.js is {size} bytes gzipped; the budget is {BUDGET}"
        );
    }

    #[tokio::test]
    async fn test_extras_script_is_served_like_the_tracker() {
        let response = serve_extras_script().await;
        assert_eq!(response.status(), StatusCode::OK);
        let h = |name| response.headers().get(name).unwrap().to_str().unwrap();
        assert_eq!(h(header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
        assert!(h(header::CONTENT_TYPE).contains("javascript"));
        assert_eq!(h(header::CACHE_CONTROL), "public, max-age=3600");
    }

    #[test]
    fn test_extras_script_is_not_the_main_tracker_and_has_no_storage_writes() {
        assert!(EXTRAS_SCRIPT.contains("window.mallard"));
        assert!(
            !EXTRAS_SCRIPT.contains("sendBeacon"),
            "sends via the tracker"
        );
        for forbidden in [
            "localStorage",
            "sessionStorage",
            "document.cookie",
            "indexedDB",
        ] {
            assert!(
                !EXTRAS_SCRIPT.contains(forbidden),
                "extras must not touch {forbidden}"
            );
        }
    }
}
