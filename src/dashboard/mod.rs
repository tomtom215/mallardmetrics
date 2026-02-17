use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/dashboard/assets/"]
struct Assets;

/// Serve embedded static files for the dashboard SPA.
pub async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    serve_file(&path)
}

/// Serve the index.html for the root path.
pub async fn serve_index() -> impl IntoResponse {
    serve_file("index.html")
}

fn serve_file(path: &str) -> impl IntoResponse {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
