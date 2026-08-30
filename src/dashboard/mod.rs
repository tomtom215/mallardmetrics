use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/dashboard/assets/"]
struct Assets;

/// The tracking script, compiled into the binary from its single source.
///
/// It used to exist twice — once under `tracking/` for documentation and once
/// under the embedded asset directory — with nothing keeping the copies in
/// step. Serving it from `include_str!` means there is one file, and CI fails
/// if a second copy reappears.
pub const TRACKING_SCRIPT: &str = include_str!("../../tracking/script.js");

/// How long a browser may cache a hashed dashboard asset.
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// How long a browser may cache the tracking script.
///
/// Short enough that a fix reaches sites within the hour, long enough to avoid
/// a request per pageview.
const SCRIPT_CACHE_CONTROL: &str = "public, max-age=3600";

/// Serve an embedded dashboard asset.
pub async fn serve_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    serve_file(&path)
}

/// Serve the dashboard shell.
pub async fn serve_index() -> Response {
    serve_file("index.html")
}

/// Serve the tracking script.
pub async fn serve_tracking_script() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, SCRIPT_CACHE_CONTROL),
            // The tracker is meant to be embedded by any site.
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        TRACKING_SCRIPT,
    )
        .into_response()
}

/// Look up an embedded file and build its response.
///
/// Returns `Response` rather than `impl IntoResponse`: under edition 2024 an
/// `impl Trait` return captures the lifetimes of all its arguments, so
/// borrowing `path` here would tie the returned value to it.
fn serve_file(path: &str) -> Response {
    let Some(content) = Assets::get(path) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // index.html is the entry point and must be revalidated, or a deploy would
    // leave browsers loading a stale shell that references removed assets.
    let cache_control = if path == "index.html" {
        "no-cache"
    } else {
        ASSET_CACHE_CONTROL
    };

    // rust_embed exposes a content hash; using it as the ETag lets a browser
    // revalidate cheaply instead of re-downloading.
    let etag = format!("\"{}\"", hex::encode(&content.metadata.sha256_hash()[..8]));

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache_control.to_string()),
            (header::ETAG, etag),
        ],
        content.data.to_vec(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn test_index_is_served() {
        let response = serve_index().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_string(response).await.contains("<div id=\"app\">"));
    }

    #[tokio::test]
    async fn test_index_is_not_cached_immutably() {
        // An immutable index.html would strand browsers on a stale shell after
        // a deploy.
        let response = serve_index().await;
        let cache = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cache, "no-cache");
    }

    #[tokio::test]
    async fn test_static_assets_are_cached_and_tagged() {
        let response = serve_asset(axum::extract::Path("style.css".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("immutable")
        );
        assert!(headers.contains_key(header::ETAG));
        assert!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("text/css")
        );
    }

    #[tokio::test]
    async fn test_missing_asset_is_not_found() {
        let response = serve_asset(axum::extract::Path("nope.js".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_tracking_script_is_served_with_cors() {
        let response = serve_tracking_script().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "the tracker must be loadable from any site"
        );
        assert!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("javascript")
        );
    }

    #[tokio::test]
    async fn test_tracking_script_content_is_the_real_tracker() {
        let body = body_string(serve_tracking_script().await).await;
        assert!(body.contains("data-domain"));
        assert!(body.contains("sendBeacon"));
    }

    #[test]
    fn test_tracking_script_is_embedded_from_a_single_source() {
        assert!(!TRACKING_SCRIPT.is_empty());
        // A second copy in the asset directory would drift out of step.
        assert!(
            Assets::get("mallard.js").is_none(),
            "the tracker must not also live in the embedded asset directory"
        );
    }

    #[tokio::test]
    async fn test_asset_lookup_does_not_escape_the_bundle() {
        // rust_embed matches exact keys, so traversal simply misses; assert it.
        for path in ["../Cargo.toml", "..%2FCargo.toml", "/etc/passwd"] {
            let response = serve_asset(axum::extract::Path(path.to_string())).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn test_body_is_returned_for_a_known_asset() {
        let response = serve_asset(axum::extract::Path("app.js".to_string())).await;
        let _ = Body::new(axum::body::Body::empty());
        assert!(body_string(response).await.contains("render("));
    }
}
