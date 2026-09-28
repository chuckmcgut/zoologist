//! The dashboard's files, built into the program so the page works wherever Zoologist runs from.
//! A `server.static_dir` that holds an `index.html` is served instead, to customise the page.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

/// Path, content type and bytes of each built-in file.
const FILES: [(&str, &str, &[u8]); 4] = [
    (
        "index.html",
        "text/html; charset=utf-8",
        include_bytes!("../../../../static/index.html"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../../../static/app.js"),
    ),
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../../../../static/style.css"),
    ),
    (
        "favicon.svg",
        "image/svg+xml",
        include_bytes!("../../../../static/favicon.svg"),
    ),
];

/// Serves the built-in files; `/` is `index.html`.
pub async fn built_in(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match FILES.iter().find(|(name, _, _)| *name == path) {
        Some((_, content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                // Revalidate every time: a new version must show up after an update.
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
