use ratatui::text::Line;

use crate::relish::tui::{app::TuiApp, keys::KEYBINDINGS};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = vec![widgets::heading("KEY             CONTEXT       ACTION")];
    for (key, context, description) in KEYBINDINGS.iter().skip(app.events_scroll).take(32) {
        lines.push(Line::raw(format!("{key:<15} {context:<13} {description}")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::tui::{
        app::{DetailTab, LogLine, View},
        fixtures::{TestScenario, render_to_string},
        msg::Msg,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn small_terminal_is_deterministic() {
        let app = TuiApp::with_test_data(TestScenario::Empty);
        insta::assert_snapshot!("terminal_too_small", render_to_string(&app, 60, 15));
    }

    #[test]
    fn healthy_dashboard_snapshot() {
        let app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        insta::assert_snapshot!("dashboard_healthy", render_to_string(&app, 120, 40));
    }

    #[test]
    fn help_lists_keybindings() {
        let mut app = TuiApp::with_test_data(TestScenario::Empty);
        app.view_stack.push(View::Help);
        let output = render_to_string(&app, 120, 40);
        assert!(output.contains("Shift-Tab"));
        assert!(output.contains("command palette"));
    }

    #[test]
    fn phase_thirteen_view_snapshots() {
        let mut app = TuiApp::with_test_data(TestScenario::Empty);
        insta::assert_snapshot!("dashboard_empty", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::DegradedApp);
        insta::assert_snapshot!("dashboard_degraded", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::ManyApps);
        app.view_stack.push(View::Apps);
        insta::assert_snapshot!("apps_many", render_to_string(&app, 120, 40));
        app.filter = Some("web".into());
        insta::assert_snapshot!("apps_filtered", render_to_string(&app, 120, 40));

        for tab in DetailTab::ALL {
            app = TuiApp::with_test_data(TestScenario::HealthyCluster);
            app.view_stack.push(View::AppDetail {
                app: "web".into(),
                namespace: "default".into(),
                tab,
            });
            if tab == DetailTab::Logs {
                app.log_lines.push_back(LogLine {
                    instance: "web-0".into(),
                    line: "server ready".into(),
                });
            }
            insta::assert_snapshot!(
                format!("app_detail_{tab:?}").to_lowercase(),
                render_to_string(&app, 120, 40)
            );
        }

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Nodes);
        insta::assert_snapshot!("nodes", render_to_string(&app, 120, 40));
        app.view_stack.push(View::NodeDetail {
            node: "node-1".into(),
        });
        insta::assert_snapshot!("node_detail", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Jobs);
        insta::assert_snapshot!("jobs", render_to_string(&app, 120, 40));
        app.view_stack.push(View::JobDetail {
            name: "migrate".into(),
            namespace: "default".into(),
        });
        insta::assert_snapshot!("job_detail", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Events);
        insta::assert_snapshot!("events", render_to_string(&app, 120, 40));

        app.view_stack.push(View::Logs {
            app: Some(("web".into(), "default".into())),
        });
        app.log_lines.push_back(LogLine {
            instance: "web-0".into(),
            line: "request completed".into(),
        });
        insta::assert_snapshot!("logs", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Routes);
        insta::assert_snapshot!("routes", render_to_string(&app, 120, 40));
        app.view_stack.push(View::RouteDetail {
            host: "web.example.test".into(),
        });
        insta::assert_snapshot!("route_detail", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Help);
        insta::assert_snapshot!("help", render_to_string(&app, 120, 40));

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Search);
        insta::assert_snapshot!("search_empty", render_to_string(&app, 120, 40));
        for character in "web".chars() {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )));
        }
        insta::assert_snapshot!("search_web", render_to_string(&app, 120, 40));
    }
}
