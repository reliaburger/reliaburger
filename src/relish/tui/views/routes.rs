use ratatui::text::Line;

use crate::relish::tui::app::{TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::RouteDetail { host } => {
            let Some(route) = app.data.routes.iter().find(|route| &route.host == host) else {
                return vec![Line::raw("route not found")];
            };
            vec![
                widgets::heading(&route.host),
                Line::raw(format!("path             {}", route.path)),
                Line::raw(format!("app              {}", route.app_name)),
                Line::raw(format!(
                    "backends         {}/{}",
                    route.healthy_backends, route.total_backends
                )),
                Line::raw(format!("websocket        {}", route.websocket)),
            ]
        }
        _ => {
            let mut lines = vec![widgets::heading(
                "HOST                         PATH       APP                  BACKENDS  WS",
            )];
            if app.data.routes.is_empty() {
                lines.push(Line::raw("no ingress routes configured"));
            }
            for (index, route) in app.data.routes.iter().enumerate() {
                lines.push(widgets::row(
                    format!(
                        "{:<28} {:<10} {:<20} {:>2}/{:<2}     {}",
                        route.host,
                        route.path,
                        route.app_name,
                        route.healthy_backends,
                        route.total_backends,
                        if route.websocket { "yes" } else { "no" }
                    ),
                    index == app.routes_cursor,
                ));
            }
            lines
        }
    }
}
