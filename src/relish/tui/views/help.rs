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
        app::{DetailTab, LogLine, NodeTab, View},
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
    fn a_node_that_did_not_answer_makes_the_dashboard_say_partial() {
        let app = TuiApp::with_test_data(TestScenario::NodeMissing);
        let output = render_to_string(&app, 120, 40);
        insta::assert_snapshot!("dashboard_partial", output);
        assert!(
            output.contains("2 nodes, partial (2 of 3 nodes)"),
            "the header says it saw less: {output}"
        );
        assert!(
            output.contains("2 alive, 0 suspect, 0 dead; partial (2 of 3 nodes)"),
            "{output}"
        );
        assert!(output.contains("Cluster DEGRADED"), "{output}");
    }

    #[test]
    fn a_missing_section_reads_unknown_and_the_dashboard_waits_for_a_summary() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        if let Some(summary) = app.data.summary.as_mut() {
            summary.capacity = crate::bun::summary::Section::missing("leader: timed out");
        }
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("capacity   unknown: leader: timed out"),
            "{output}"
        );
        assert!(output.contains("Cluster DEGRADED"), "{output}");
        app.data.summary = None;
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("waiting for the cluster summary"),
            "{output}"
        );
    }

    // Mid-rollout an app's instances run different images; the Overview
    // names each one instead of picking one (#2167).
    #[test]
    fn app_detail_overview_lists_every_image_mid_rollout() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        let second = app
            .data
            .instances
            .iter_mut()
            .find(|row| row.instance.id == "web-1")
            .unwrap();
        second.instance.image = Some("registry.example/web:v2".into());
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Overview,
        });
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("image          registry.example/web:v1, registry.example/web:v2"),
            "{output}"
        );
    }

    // An older node, or a process workload, reports no image.
    #[test]
    fn app_detail_overview_says_unknown_without_an_image() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        for row in &mut app.data.instances {
            row.instance.image = None;
        }
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Overview,
        });
        let output = render_to_string(&app, 120, 40);
        assert!(output.contains("image          unknown"), "{output}");
    }

    /// An app's log view with four lines, one of them a warning.
    fn logs_in(view: View) -> TuiApp {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(view);
        for (instance, line) in [
            ("web-0", "GET /health 200 in 3ms"),
            ("web-1", "ERROR: disk full on /var/lib/web"),
            ("web-0", "retrying after error, attempt 2"),
            ("web-1", "server ready"),
        ] {
            app.log_lines.push_back(LogLine {
                instance: instance.into(),
                line: line.into(),
            });
        }
        app
    }

    fn logs_tab() -> View {
        View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Logs,
        }
    }

    fn logs_view() -> View {
        View::Logs {
            app: Some(("web".into(), "default".into())),
        }
    }

    // `/` in the Logs tab and the logs view shows only the matching lines
    // and says how many of how many (#2174).
    #[test]
    fn log_search_snapshots() {
        for (name, view) in [("app_detail_logs", logs_tab()), ("logs", logs_view())] {
            let mut app = logs_in(view);
            app.filter = Some("error".into());
            let output = render_to_string(&app, 120, 40);
            assert!(output.contains("filter: error (2 of 4 lines)"), "{output}");
            assert!(!output.contains("server ready"), "{output}");
            insta::assert_snapshot!(format!("{name}_search_some"), output);

            app.filter = Some("panic".into());
            let output = render_to_string(&app, 120, 40);
            assert!(output.contains("filter: panic (0 of 4 lines)"), "{output}");
            assert!(output.contains("no lines match"), "{output}");
            insta::assert_snapshot!(format!("{name}_search_none"), output);
        }

        let mut app = logs_in(logs_tab());
        app.filter = Some("health".into());
        app.filter_editing = true;
        insta::assert_snapshot!(
            "app_detail_logs_search_mid_line",
            render_to_string(&app, 120, 40)
        );
    }

    /// The first cell of `text` on screen and its style, from a render that
    /// keeps the styles `render_to_string` drops.
    fn styled_cell(app: &TuiApp, text: &str) -> ratatui::style::Style {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::super::render(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = render_to_string(app, 120, 40);
        let (y, row) = rows
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(text))
            .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{rows}"));
        let x = row[..row.find(text).unwrap()].chars().count();
        buffer[(x as u16, y as u16)].style()
    }

    #[test]
    fn a_search_match_is_red_and_bold_and_the_rest_of_the_line_is_not() {
        use ratatui::style::{Color, Modifier};

        let mut app = logs_in(logs_tab());
        app.filter = Some("HEALTH".into());
        let matched = styled_cell(&app, "health 200");
        assert_eq!(matched.fg, Some(Color::Red));
        assert!(matched.add_modifier.contains(Modifier::BOLD));
        let before = styled_cell(&app, "GET /health");
        assert_ne!(before.fg, Some(Color::Red));
        let after = styled_cell(&app, "200 in 3ms");
        assert_ne!(after.fg, Some(Color::Red));
    }

    // Without a search the Metrics tab lists every series the app exports,
    // not only CPU and memory; with one, the series whose name or labels
    // match (#2174).
    #[test]
    fn metrics_search_snapshots() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Metrics,
        });
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("http_requests_total{route=\"/health\"}"),
            "{output}"
        );

        app.filter = Some("process".into());
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("filter: process (2 of 3 metrics)"),
            "{output}"
        );
        assert!(!output.contains("http_requests_total"), "{output}");
        insta::assert_snapshot!("app_detail_metrics_search_some", output);

        // Labels count: `health` is only in the route label.
        app.filter = Some("health".into());
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains("filter: health (1 of 3 metrics)"),
            "{output}"
        );
        assert!(output.contains("http_requests_total"), "{output}");

        app.filter = Some("disk".into());
        let output = render_to_string(&app, 120, 40);
        assert!(output.contains("no metrics match"), "{output}");
        insta::assert_snapshot!("app_detail_metrics_search_none", output);
    }

    // The Instances tab asks before it kills (#2175).
    #[test]
    fn kill_confirmation_snapshot() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Instances,
        });
        app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::NONE,
        )));
        let output = render_to_string(&app, 120, 40);
        assert!(
            output.contains(
                "Kill instance web-1 on node-2? The scheduler starts a replacement. [y/N]"
            ),
            "{output}"
        );
        insta::assert_snapshot!("app_detail_instances_confirm_kill", output);
    }

    #[test]
    fn the_selected_instance_is_highlighted() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Instances,
        });
        app.instances_cursor = 1;
        assert_eq!(
            styled_cell(&app, "node-2       web-1").bg,
            crate::relish::tui::theme::selected().bg
        );
        assert_ne!(
            styled_cell(&app, "node-1       web-0").bg,
            crate::relish::tui::theme::selected().bg
        );
    }

    /// node-1 of the healthy fixture, plus a single-replica `cache` that
    /// keeps data on node-1 and a third `web` replica on node-3, with one
    /// of three voters already silent: every kind of impact at once.
    fn node_with_every_impact(tab: NodeTab) -> TuiApp {
        use crate::bun::agent::ClusterInstanceStatus;
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        let mut cache: ClusterInstanceStatus = app.data.instances[0].clone();
        cache.instance.app_name = "cache".into();
        cache.instance.id = "cache-0".into();
        cache.instance.image = Some("registry.example/cache:v2".into());
        cache.instance.data_on_node = true;
        let mut web = app.data.instances[1].clone();
        web.node = "node-3".into();
        web.instance.id = "web-2".into();
        app.data.instances.extend([cache, web]);
        if let Some(council) = app
            .data
            .summary
            .as_mut()
            .and_then(|summary| summary.council.value.as_mut())
        {
            council.voters_answering = 2;
        }
        app.view_stack.push(View::Nodes);
        app.view_stack.push(View::NodeDetail {
            node: "node-1".into(),
            tab,
        });
        app
    }

    // The node page at the smallest terminal the TUI draws in (#2170).
    #[test]
    fn node_detail_tabs_at_80_by_25() {
        for tab in NodeTab::ALL {
            let app = node_with_every_impact(tab);
            insta::assert_snapshot!(
                format!("node_detail_80x25_{tab:?}").to_lowercase(),
                render_to_string(&app, 80, 25)
            );
        }
    }

    #[test]
    fn the_maintenance_tab_flags_a_single_replica_app_and_a_lost_quorum() {
        let app = node_with_every_impact(NodeTab::Maintenance);
        let screen = render_to_string(&app, 120, 40);
        let line = |needle: &str| {
            screen
                .lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no {needle}:\n{screen}"))
                .to_string()
        };
        let cache = line("cache ");
        assert!(cache.contains("goes down"), "{cache}");
        assert!(cache.contains("here"), "{cache}");
        assert!(line("worker ").contains("loses redundancy"));
        assert!(line("web ").contains("keeps serving"));
        assert!(line("council ").contains("loses quorum"));
    }

    #[test]
    fn the_workloads_tab_lists_only_that_nodes_instances() {
        let app = node_with_every_impact(NodeTab::Workloads);
        let screen = render_to_string(&app, 120, 40);
        for id in ["web-0", "worker-0", "app-2-0", "cache-0"] {
            assert!(screen.contains(id), "{id} missing:\n{screen}");
        }
        for id in ["web-1", "web-2", "worker-1"] {
            assert!(!screen.contains(id), "{id} is on another node:\n{screen}");
        }
        assert!(screen.contains("registry.example/cache:v2"), "{screen}");
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
        for tab in NodeTab::ALL {
            app.view_stack.push(View::NodeDetail {
                node: "node-1".into(),
                tab,
            });
            insta::assert_snapshot!(
                format!("node_detail_{tab:?}").to_lowercase(),
                render_to_string(&app, 120, 40)
            );
            app.view_stack.pop();
        }

        app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Jobs);
        insta::assert_snapshot!("jobs", render_to_string(&app, 120, 40));
        app.view_stack.push(View::JobDetail {
            name: "migrate".into(),
            namespace: "default".into(),
            node: "node-1".into(),
            instance: "migrate-0".into(),
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
