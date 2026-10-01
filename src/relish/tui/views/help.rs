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
    fn log_views_keep_the_tail_visible_at_short_and_tall_terminal_heights() {
        for view in [
            View::Logs {
                app: Some(("web".into(), "default".into())),
            },
            View::AppDetail {
                app: "web".into(),
                namespace: "default".into(),
                tab: DetailTab::Logs,
            },
        ] {
            let mut app = TuiApp::new();
            app.view_stack.push(view);
            for index in 0..100 {
                app.log_lines.push_back(LogLine {
                    instance: "web".into(),
                    line: format!("MSG-{index:03}"),
                });
            }
            assert!(render_to_string(&app, 80, 24).contains("MSG-099"));
            assert!(render_to_string(&app, 120, 80).contains("MSG-040"));
            app.log_scroll = usize::MAX;
            assert!(render_to_string(&app, 80, 24).contains("MSG-000"));
        }
    }

    /// Every cluster-wide view names the node behind each row and says which
    /// members are missing, so a partial list never passes for a whole one.
    #[test]
    fn cluster_views_show_each_rows_node_and_name_missing_members() {
        use crate::bun::cluster_view::{ClusterDeployHistory, NodeTagged};
        use crate::relish::tui::msg::StreamItem;

        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        let at = std::time::SystemTime::UNIX_EPOCH;
        let entry = |node: &str| NodeTagged {
            node: node.to_string(),
            row: crate::meat::deploy_types::DeployHistoryEntry {
                id: crate::meat::deploy_types::DeployId(4),
                app_id: crate::meat::types::AppId::new("web", "default"),
                image: "web:v4".into(),
                result: crate::meat::deploy_types::DeployResult::Completed,
                created_at: at,
                completed_at: at,
                steps_completed: 1,
                steps_total: 1,
                spec: None,
            },
        };
        app.data.deploy_history.insert(
            crate::relish::tui::state::deploy_history_key("web", "default"),
            ClusterDeployHistory {
                app: "web".into(),
                namespace: "default".into(),
                history: vec![entry("node-1"), entry("node-2")],
                warnings: vec!["node node-3 timed out".into()],
            },
        );
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Deploys,
        });
        let deploys = render_to_string(&app, 120, 40);
        assert!(deploys.contains("node-1"), "{deploys}");
        assert!(deploys.contains("node-2"), "{deploys}");
        assert!(deploys.contains("incomplete: node node-3 timed out"));

        app.data.job_warnings = vec!["node node-3 timed out".into()];
        app.view_stack.push(View::Jobs);
        assert!(render_to_string(&app, 120, 40).contains("incomplete: node node-3"));

        app.data.event_warnings = vec!["node node-2: connection refused".into()];
        app.view_stack.push(View::Events);
        assert!(render_to_string(&app, 120, 40).contains("incomplete: node node-2"));

        app.view_stack.push(View::Logs {
            app: Some(("web".into(), "default".into())),
        });
        app.update(Msg::Stream(StreamItem::LogWarning(
            "node node-3 left the cluster; no longer following its logs".into(),
        )));
        assert!(
            render_to_string(&app, 120, 40)
                .contains("warning │ node node-3 left the cluster; no longer following its logs")
        );
    }

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
