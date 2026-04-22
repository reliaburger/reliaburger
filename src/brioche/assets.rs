//! Static asset serving via `rust-embed`.
//!
//! HTMX, uPlot, and custom JS/CSS are vendored in `brioche/dist/`
//! and compiled into the binary at build time. No filesystem
//! dependency at runtime.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "brioche/dist/"]
struct BriocheAssets;

/// Serve a static asset from the embedded `brioche/dist/` directory.
///
/// Registered as `GET /ui/static/*path`.
pub async fn static_asset_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    match BriocheAssets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, mime.as_ref().parse().unwrap());
            headers.insert(
                header::CACHE_CONTROL,
                "public, max-age=86400".parse().unwrap(),
            );
            (StatusCode::OK, headers, content.data.to_vec()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_contain_htmx() {
        assert!(BriocheAssets::get("htmx.min.js").is_some());
    }

    #[test]
    fn embedded_assets_contain_uplot() {
        assert!(BriocheAssets::get("uplot.min.js").is_some());
        assert!(BriocheAssets::get("uplot.min.css").is_some());
    }

    #[test]
    fn embedded_assets_contain_brioche() {
        assert!(BriocheAssets::get("brioche.js").is_some());
        assert!(BriocheAssets::get("brioche.css").is_some());
    }

    #[test]
    fn missing_asset_returns_none() {
        assert!(BriocheAssets::get("nonexistent.js").is_none());
    }
}
