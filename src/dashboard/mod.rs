use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

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

/// Cache policy for an asset requested at its versioned URL.
///
/// The URL carries a hash of the whole bundle (see [`Bundle`]), so the bytes
/// behind it can never change and the browser may keep them for good. A repeat
/// visit then makes no request for them at all, where it used to make one
/// conditional request per file (six of them) on every load.
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Cache policy for an asset requested at a bare path (`/app.js`).
///
/// Revalidating rather than `immutable`: the path is unversioned, so a cached
/// copy would outlive the binary that produced it. The ETag makes revalidation
/// a bodyless 304. Only a stale bookmark or an old `index.html` asks this way.
const REVALIDATE_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate";
/// Cache policy for the entry page.
///
/// It names the versioned URLs, so it must be revalidated on every load: that
/// one conditional request is what carries a deploy to returning visitors.
const INDEX_CACHE_CONTROL: &str = "no-cache";
/// How long a browser may cache the tracking script.
///
/// Short enough that a fix reaches sites within the hour, long enough to avoid
/// a request per pageview.
pub(crate) const SCRIPT_CACHE_CONTROL: &str = "public, max-age=3600";

/// One embedded file, ready to serve.
struct Asset {
    body: Vec<u8>,
    mime: String,
    /// Strong validator over exactly these bytes, quoted.
    etag: String,
}

/// Every embedded file with its references rewritten to versioned URLs.
///
/// The dashboard is several files, so a plain `max-age` would let a browser
/// pair a new `app.js` with a cached older `format.js` after an upgrade, and a
/// missing export is a link error that blanks the page. Instead the bundle gets
/// one version, a hash of every file, and `index.html` and the JavaScript modules
/// name their siblings as `/format.js?v=<version>`. Change any byte of any file
/// and every URL changes with it, so no mixed state is reachable, and nothing
/// needs a build step: the rewrite happens once, at first request.
struct Bundle {
    version: String,
    files: HashMap<String, Asset>,
}

fn bundle() -> &'static Bundle {
    static BUNDLE: OnceLock<Bundle> = OnceLock::new();
    BUNDLE.get_or_init(Bundle::build)
}

impl Bundle {
    fn build() -> Self {
        let mut raw: Vec<(String, Vec<u8>)> = Assets::iter()
            .filter_map(|name| {
                let file = Assets::get(&name)?;
                Some((name.into_owned(), file.data.into_owned()))
            })
            .collect();
        raw.sort_by(|a, b| a.0.cmp(&b.0));

        // Hashed from the files as embedded, before any rewriting, so the
        // version is a function of the sources alone.
        let mut hasher = Sha256::new();
        for (name, data) in &raw {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update((data.len() as u64).to_le_bytes());
            hasher.update(data);
        }
        let version = hex::encode(hasher.finalize())[..12].to_string();

        let names: Vec<&str> = raw
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| *name != "index.html")
            .collect();
        let files = raw
            .iter()
            .map(|(name, data)| {
                let rewritable = std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "html" || ext == "js");
                let body = if rewritable {
                    version_references(data, &names, &version)
                } else {
                    data.clone()
                };
                let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&body)[..8]));
                let mime = mime_guess::from_path(name).first_or_octet_stream();
                (
                    name.clone(),
                    Asset {
                        body,
                        mime: mime.as_ref().to_string(),
                        etag,
                    },
                )
            })
            .collect();
        Self { version, files }
    }
}

/// Append `?v=<version>` to every quoted reference to a bundled file.
///
/// Matches only an exact quoted path (`"/format.js"` or `'/format.js'`), which is
/// how `index.html` and the modules' `import`s spell them, so API paths and
/// prose that merely contain a file name are left alone.
fn version_references(source: &[u8], names: &[&str], version: &str) -> Vec<u8> {
    let Ok(mut text) = String::from_utf8(source.to_vec()) else {
        return source.to_vec();
    };
    for name in names {
        for quote in ['"', '\''] {
            let from = format!("{quote}/{name}{quote}");
            let to = format!("{quote}/{name}?v={version}{quote}");
            text = text.replace(&from, &to);
        }
    }
    text.into_bytes()
}

/// Serve an embedded dashboard asset.
pub async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    serve_file(&path, query.as_deref(), &headers)
}

/// Serve the dashboard shell.
pub async fn serve_index(headers: HeaderMap) -> Response {
    serve_file("index.html", None, &headers)
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

/// Is `query` asking for exactly this bundle's version?
fn names_current_version(query: Option<&str>, version: &str) -> bool {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .any(|pair| pair.strip_prefix("v=") == Some(version))
}

/// Look up an embedded file and build its response.
///
/// Returns `Response` rather than `impl IntoResponse`: under edition 2024 an
/// `impl Trait` return captures the lifetimes of all its arguments, so
/// borrowing `path` here would tie the returned value to it.
fn serve_file(path: &str, query: Option<&str>, request_headers: &HeaderMap) -> Response {
    let bundle = bundle();
    let Some(asset) = bundle.files.get(path) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };

    let cache_control = if path == "index.html" {
        INDEX_CACHE_CONTROL
    } else if names_current_version(query, &bundle.version) {
        // Only the version this binary emitted is immutable. A stale `?v=` from
        // an older page still gets the current bytes, but must be revalidated,
        // or the browser would pin them under a URL that means something else.
        IMMUTABLE_CACHE_CONTROL
    } else {
        REVALIDATE_CACHE_CONTROL
    };

    if if_none_match_matches(request_headers, &asset.etag) {
        // 304 carries no body, and RFC 9110 §15.4.5 asks for the headers that
        // would have been sent with a 200 — the client needs them to refresh
        // what it has stored.
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, asset.etag.clone()),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.mime.clone()),
            (header::CACHE_CONTROL, cache_control.to_string()),
            (header::ETAG, asset.etag.clone()),
        ],
        asset.body.clone(),
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
    use axum::extract::{Path, RawQuery};
    use http_body_util::BodyExt;

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn asset(path: &str, query: Option<&str>, headers: HeaderMap) -> Response {
        serve_asset(
            Path(path.to_string()),
            RawQuery(query.map(str::to_string)),
            headers,
        )
        .await
    }

    fn header_str(response: &Response, name: header::HeaderName) -> &str {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    fn version() -> &'static str {
        &bundle().version
    }

    #[tokio::test]
    async fn test_index_is_served() {
        let response = serve_index(HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_string(response).await.contains("<div id=\"app\">"));
    }

    #[tokio::test]
    async fn test_index_is_always_revalidated() {
        // The index names the versioned URLs; if it were cached, a deploy would
        // strand browsers on a shell that points at the previous release.
        let response = serve_index(HeaderMap::new()).await;
        assert_eq!(header_str(&response, header::CACHE_CONTROL), "no-cache");
        assert!(response.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn test_index_names_every_asset_by_versioned_url() {
        let body = body_string(serve_index(HeaderMap::new()).await).await;
        let v = version();
        for name in [
            "theme.js",
            "style.css",
            "preact.js",
            "htm.js",
            "format.js",
            "api.js",
            "app.js",
            "favicon.svg",
        ] {
            assert!(
                body.contains(&format!("\"/{name}?v={v}\"")),
                "index.html must reference {name} with the bundle version"
            );
            assert!(
                !body.contains(&format!("\"/{name}\"")),
                "index.html still has an unversioned reference to {name}"
            );
        }
    }

    #[tokio::test]
    async fn test_modules_import_their_siblings_by_versioned_url() {
        // Mixing a new app.js with a cached old format.js is a link error, so
        // every import has to carry the version too.
        let v = version();
        let app = body_string(asset("app.js", None, HeaderMap::new()).await).await;
        for name in ["preact.js", "htm.js", "format.js", "api.js"] {
            assert!(
                app.contains(&format!("'/{name}?v={v}'")),
                "app.js must import {name} by its versioned URL"
            );
            assert!(
                !app.contains(&format!("'/{name}'")),
                "app.js still imports {name} unversioned"
            );
        }
        // API paths that merely resemble nothing in the bundle are untouched.
        assert!(app.contains("/api/stats/main"));
        assert!(!app.contains("/api/stats/main?v="));
    }

    #[tokio::test]
    async fn test_every_versioned_reference_resolves() {
        let v = version();
        let index = body_string(serve_index(HeaderMap::new()).await).await;
        let needle = format!("?v={v}\"");
        let mut checked = 0;
        for part in index.split('"') {
            if let Some(path) = part.strip_suffix(&format!("?v={v}")) {
                let name = path.trim_start_matches('/');
                let response = asset(name, Some(&format!("v={v}")), HeaderMap::new()).await;
                assert_eq!(response.status(), StatusCode::OK, "{name}");
                checked += 1;
            }
        }
        assert!(index.contains(&needle));
        assert!(checked >= 6, "expected the index to reference the bundle");
    }

    #[tokio::test]
    async fn test_versioned_assets_are_immutable() {
        let v = version();
        let response = asset("style.css", Some(&format!("v={v}")), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL),
            "public, max-age=31536000, immutable"
        );
        assert!(header_str(&response, header::CONTENT_TYPE).contains("text/css"));
        assert!(response.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn test_unversioned_or_stale_assets_are_revalidated_not_pinned() {
        // The bare path means different bytes after an upgrade, and a stale
        // `?v=` must not be pinned to whatever happens to be served now.
        for query in [None, Some("v=000000000000"), Some("x=1"), Some("v=")] {
            let response = asset("style.css", query, HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::OK, "{query:?}");
            assert_eq!(
                header_str(&response, header::CACHE_CONTROL),
                "public, max-age=0, must-revalidate",
                "{query:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_version_is_stable_and_tracks_content() {
        // Stable across calls (and so across restarts of the same binary), and
        // a function of the content: the ETag differs between files.
        assert_eq!(version(), bundle().version);
        assert_eq!(version().len(), 12);
        assert!(version().chars().all(|c| c.is_ascii_hexdigit()));
        let a = asset("style.css", None, HeaderMap::new()).await;
        let b = asset("api.js", None, HeaderMap::new()).await;
        assert_ne!(
            header_str(&a, header::ETAG),
            header_str(&b, header::ETAG),
            "different files must not share a validator"
        );
    }

    #[test]
    fn test_references_are_rewritten_only_when_exactly_quoted() {
        let out = version_references(
            b"import a from '/a.js'; fetch('/api/a.js/x'); s=\"/a.js\"; t='/a.json'",
            &["a.js"],
            "V",
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "import a from '/a.js?v=V'; fetch('/api/a.js/x'); s=\"/a.js?v=V\"; t='/a.json'"
        );
    }

    #[test]
    fn test_names_current_version() {
        assert!(names_current_version(Some("v=abc"), "abc"));
        assert!(names_current_version(Some("x=1&v=abc"), "abc"));
        assert!(!names_current_version(Some("v=abcd"), "abc"));
        assert!(!names_current_version(Some("xv=abc"), "abc"));
        assert!(!names_current_version(None, "abc"));
    }

    #[tokio::test]
    async fn test_etag_is_strong_and_matches_the_served_bytes() {
        let response = asset("app.js", None, HeaderMap::new()).await;
        let etag = header_str(&response, header::ETAG).to_string();
        assert!(
            etag.starts_with('"') && etag.ends_with('"'),
            "a strong validator is a bare quoted string, not W/: {etag}"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            etag,
            format!("\"{}\"", hex::encode(&Sha256::digest(&body)[..8])),
            "the ETag must be derived from the bytes actually sent"
        );
    }

    #[tokio::test]
    async fn test_matching_if_none_match_gets_a_304() {
        // The same holds at the versioned and the bare URL.
        for query in [None, Some(format!("v={}", version()))] {
            let first = asset("style.css", query.as_deref(), HeaderMap::new()).await;
            let etag = header_str(&first, header::ETAG).to_string();
            let cache = header_str(&first, header::CACHE_CONTROL).to_string();

            let mut conditional = HeaderMap::new();
            conditional.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
            let second = asset("style.css", query.as_deref(), conditional).await;

            assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(header_str(&second, header::ETAG), etag);
            assert_eq!(header_str(&second, header::CACHE_CONTROL), cache);
            assert!(body_string(second).await.is_empty());
        }
    }

    #[tokio::test]
    async fn test_index_revalidates_to_a_304() {
        let first = serve_index(HeaderMap::new()).await;
        let etag = header_str(&first, header::ETAG).to_string();
        let mut conditional = HeaderMap::new();
        conditional.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let second = serve_index(conditional).await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(header_str(&second, header::CACHE_CONTROL), "no-cache");
    }

    #[tokio::test]
    async fn test_stale_if_none_match_gets_the_body() {
        let mut stale = HeaderMap::new();
        stale.insert(
            header::IF_NONE_MATCH,
            "\"0000000000000000\"".parse().unwrap(),
        );
        let response = asset("style.css", None, stale).await;
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
        let response = asset("nope.js", None, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_tracking_script_is_served_with_cors() {
        let response = serve_tracking_script().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            "*",
            "the tracker must be loadable from any site"
        );
        assert!(header_str(&response, header::CONTENT_TYPE).contains("javascript"));
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL),
            "public, max-age=3600"
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
        // The bundle is a map of exact keys, so traversal simply misses.
        for path in ["../Cargo.toml", "..%2FCargo.toml", "/etc/passwd"] {
            let response = asset(path, None, HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn test_body_is_returned_for_a_known_asset() {
        let response = asset("app.js", None, HeaderMap::new()).await;
        let _ = Body::new(axum::body::Body::empty());
        assert!(body_string(response).await.contains("render("));
    }
}
