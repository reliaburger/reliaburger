//! The dashboard login page.
//!
//! A minimal server-rendered form: the operator pastes an API token, which is
//! POSTed to `/ui/session` and exchanged for a read-only session cookie. This
//! exists because a browser can't attach a bearer token to an `EventSource`
//! or an `hx-get`, so the dashboard authenticates by cookie instead.

/// Render the login page. `error` shows a message above the form (e.g. after
/// a rejected token).
pub fn render_login(error: Option<&str>) -> String {
    let error_html = match error {
        Some(msg) => format!(r#"<p class="error" role="alert">{}</p>"#, html_escape(msg)),
        None => String::new(),
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Reliaburger — sign in</title>
  <style>
    body {{ font-family: system-ui, sans-serif; background: #1a1a1a; color: #eee;
            display: flex; min-height: 100vh; align-items: center; justify-content: center; margin: 0; }}
    form {{ background: #262626; padding: 2rem; border-radius: 8px; width: 20rem; }}
    h1 {{ font-size: 1.2rem; margin-top: 0; }}
    input {{ width: 100%; box-sizing: border-box; padding: 0.6rem; margin: 0.5rem 0 1rem;
             border: 1px solid #444; border-radius: 4px; background: #1a1a1a; color: #eee; }}
    button {{ width: 100%; padding: 0.6rem; border: 0; border-radius: 4px;
              background: #c0392b; color: #fff; font-weight: 600; cursor: pointer; }}
    .error {{ color: #ff6b6b; font-size: 0.9rem; }}
    .hint {{ color: #888; font-size: 0.8rem; }}
  </style>
</head>
<body>
  <form method="post" action="/ui/session">
    <h1>Reliaburger dashboard</h1>
    {error_html}
    <label for="token">API token</label>
    <input type="password" id="token" name="token" autocomplete="off" autofocus
           placeholder="rbrg_...">
    <button type="submit">Sign in</button>
    <p class="hint">Create one with <code>relish token create</code>.</p>
  </form>
</body>
</html>"#
    )
}

/// Minimal HTML escaping for text interpolated into the page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_has_a_token_form_posting_to_the_session_route() {
        let page = render_login(None);
        assert!(page.contains(r#"action="/ui/session""#));
        assert!(page.contains(r#"name="token""#));
    }

    #[test]
    fn login_page_shows_and_escapes_an_error() {
        let page = render_login(Some("bad <token>"));
        assert!(page.contains("bad &lt;token&gt;"));
        assert!(!page.contains("bad <token>"));
    }
}
