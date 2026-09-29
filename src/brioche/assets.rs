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

    fn asset_text(path: &str) -> String {
        String::from_utf8(BriocheAssets::get(path).unwrap().data.to_vec()).unwrap()
    }

    /// The value of a `--name: #rrggbb;` token in brioche.css's `:root`.
    fn token(css: &str, name: &str) -> (f64, f64, f64) {
        let root = &css[css.find(":root {").unwrap()..];
        let root = &root[..root.find('}').unwrap()];
        let start = root
            .find(&format!("{name}:"))
            .unwrap_or_else(|| panic!("{name} missing from :root"));
        let value = root[start + name.len() + 1..].trim_start();
        let hex = value.strip_prefix('#').unwrap();
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
        (channel(0), channel(2), channel(4))
    }

    /// The WCAG 2 contrast ratio between two sRGB colours.
    fn contrast(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
        let linear = |c: f64| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = |(r, g, b): (f64, f64, f64)| {
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn chart_text_meets_wcag_aa_on_every_dashboard_background() {
        let css = asset_text("brioche.css");
        for text in ["--fg", "--fg-muted"] {
            for background in ["--bg", "--panel"] {
                let ratio = contrast(token(&css, text), token(&css, background));
                assert!(
                    ratio >= 4.5,
                    "{text} on {background} is {ratio:.2}:1, below WCAG AA 4.5:1"
                );
            }
        }
    }

    #[test]
    fn chart_gridlines_stay_quieter_than_the_labels() {
        // Gridlines aren't text, so they needn't reach 4.5:1, but they must
        // show on the panel and stay fainter than the tick labels.
        let css = asset_text("brioche.css");
        let panel = token(&css, "--panel");
        let grid = contrast(token(&css, "--chart-grid"), panel);
        let labels = contrast(token(&css, "--fg-muted"), panel);
        assert!(grid >= 1.4, "gridlines at {grid:.2}:1 don't show");
        assert!(grid < labels);
    }

    #[test]
    fn charts_take_their_colours_and_units_from_the_page() {
        let js = asset_text("brioche.js");
        // Colours come from the theme tokens and units from the config's
        // ladder; nothing reads the axis title the ladder replaced.
        assert!(js.contains("\"--fg-muted\""));
        assert!(js.contains("\"--chart-grid\""));
        assert!(js.contains("cfg.unit"));
        assert!(!js.contains("y_label"));
        // An idle legend shows the newest value, not uPlot's "--".
        assert!(js.contains("idx == null ? latest("));
    }

    #[test]
    fn missing_asset_returns_none() {
        assert!(BriocheAssets::get("nonexistent.js").is_none());
    }
}
