use axum::http::{HeaderMap, StatusCode, header};
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

/// Cache policy for dashboard assets.
///
/// Deliberately revalidating rather than `immutable`. The assets are served
/// from fixed, unversioned paths (`/app.js`, `/style.css`), so the previous
/// `max-age=31536000, immutable` told every browser it could keep the old
/// dashboard for a year: upgrading the binary changed nothing a returning
/// visitor would see, and `index.html` being `no-cache` did not help, because
/// it still pointed at the same URLs.
///
/// `max-age=0, must-revalidate` plus the ETag below costs one conditional
/// request per asset per load and answers it with a 304 and no body — which is
/// what the ETag was there for, except nothing ever read `If-None-Match`, so
/// every one of those revalidations used to return the whole file.
const ASSET_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate";
/// How long a browser may cache the tracking script.
///
/// Short enough that a fix reaches sites within the hour, long enough to avoid
/// a request per pageview.
const SCRIPT_CACHE_CONTROL: &str = "public, max-age=3600";

/// Serve an embedded dashboard asset.
pub async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    serve_file(&path, &headers)
}

/// Serve the dashboard shell.
pub async fn serve_index(headers: HeaderMap) -> Response {
    serve_file("index.html", &headers)
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
fn serve_file(path: &str, request_headers: &HeaderMap) -> Response {
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

    // rust_embed exposes a content hash, which makes a strong validator: the
    // bytes are compiled into the binary, so equal hash means equal response.
    let etag = format!("\"{}\"", hex::encode(&content.metadata.sha256_hash()[..8]));

    if if_none_match_matches(request_headers, &etag) {
        // 304 carries no body, and RFC 9110 §15.4.5 asks for the headers that
        // would have been sent with a 200 — the client needs them to refresh
        // what it has stored.
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, etag),
            ],
        )
            .into_response();
    }

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

/// Does the request's `If-None-Match` cover `etag`?
///
/// Handles the list form (`"a", "b"`) and the wildcard, and tolerates the weak
/// prefix a proxy may add — a `W/` marker changes what the validator promises
/// about byte equality, not which representation it names.
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/").trim() == etag)
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
        let response = serve_index(HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_string(response).await.contains("<div id=\"app\">"));
    }

    #[tokio::test]
    async fn test_index_is_not_cached_immutably() {
        // An immutable index.html would strand browsers on a stale shell after
        // a deploy.
        let response = serve_index(HeaderMap::new()).await;
        let cache = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cache, "no-cache");
    }

    #[tokio::test]
    async fn test_static_assets_are_cached_and_tagged() {
        let response = serve_asset(
            axum::extract::Path("style.css".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        // Not `immutable`: the paths are unversioned, so a year-long cache
        // would strand a returning visitor on the previous release's dashboard.
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("public, max-age=0, must-revalidate")
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
    async fn test_matching_if_none_match_gets_a_304() {
        // The ETag existed before this and nothing read the conditional header,
        // so every "cheap revalidation" re-sent the whole file.
        let first = serve_asset(
            axum::extract::Path("style.css".to_string()),
            HeaderMap::new(),
        )
        .await;
        let etag = first
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("assets carry an ETag")
            .to_string();

        let mut conditional = HeaderMap::new();
        conditional.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let second = serve_asset(axum::extract::Path("style.css".to_string()), conditional).await;

        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(body_string(second).await.is_empty());
    }

    #[tokio::test]
    async fn test_stale_if_none_match_gets_the_body() {
        let mut stale = HeaderMap::new();
        stale.insert(
            header::IF_NONE_MATCH,
            "\"0000000000000000\"".parse().unwrap(),
        );
        let response = serve_asset(axum::extract::Path("style.css".to_string()), stale).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!body_string(response).await.is_empty());
    }

    #[test]
    fn test_if_none_match_forms() {
        let etag = "\"abc123\"";
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, value.parse().unwrap());
            if_none_match_matches(&headers, etag)
        };
        assert!(with("\"abc123\""));
        assert!(
            with("W/\"abc123\""),
            "a weak validator still names this representation"
        );
        assert!(
            with("\"other\", \"abc123\""),
            "the list form must be searched"
        );
        assert!(with("*"));
        assert!(!with("\"other\""));
        assert!(!if_none_match_matches(&HeaderMap::new(), etag));
    }

    #[tokio::test]
    async fn test_missing_asset_is_not_found() {
        let response =
            serve_asset(axum::extract::Path("nope.js".to_string()), HeaderMap::new()).await;
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
            let response =
                serve_asset(axum::extract::Path(path.to_string()), HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn test_body_is_returned_for_a_known_asset() {
        let response =
            serve_asset(axum::extract::Path("app.js".to_string()), HeaderMap::new()).await;
        let _ = Body::new(axum::body::Body::empty());
        assert!(body_string(response).await.contains("render("));
    }
}
