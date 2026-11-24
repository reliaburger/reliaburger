//! Public TUI state types and the message reducer.

pub use super::state::*;

use super::msg::{DataUpdate, Msg, StreamItem, StreamKind};

impl TuiApp {
    /// Apply one input, polling, or streaming message.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Resize(_, _) | Msg::Tick => {}
            Msg::Data(update) => self.handle_data(update),
            Msg::Stream(item) => self.handle_stream(item),
            Msg::LogStream { generation, item } if generation == self.log_generation => {
                self.handle_stream(item)
            }
            Msg::LogStream { .. } => {}
        }
    }

    pub(super) fn reset_log_stream(&mut self) -> u64 {
        self.log_generation = self.log_generation.wrapping_add(1);
        self.log_lines.clear();
        self.log_scroll = 0;
        self.log_stream_down = None;
        self.log_generation
    }

    fn handle_data(&mut self, update: DataUpdate) {
        macro_rules! set_data {
            ($result:expr, $field:ident) => {
                match $result {
                    Ok(value) => {
                        self.data.$field = value;
                        self.mark_connected();
                    }
                    Err(error) => self.mark_disconnected(error.to_string()),
                }
            };
        }
        match update {
            DataUpdate::Summary(result) => match result {
                Ok(value) => {
                    self.data.summary = Some(*value);
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::Status(result) => set_data!(result, instances),
            DataUpdate::Nodes(result) => set_data!(result, nodes),
            DataUpdate::Council(result) => match result {
                Ok(value) => {
                    self.data.council = Some(value);
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::Alerts(result) => set_data!(result, alerts),
            DataUpdate::Routes(result) => set_data!(result, routes),
            DataUpdate::Jobs(result) => match result {
                Ok(value) => {
                    self.data.jobs = value.jobs;
                    self.data.job_warnings = value.warnings;
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::EventsSeed(result) => match result {
                Ok(value) => {
                    self.data.events = value.events.into();
                    self.data.event_warnings = value.warnings;
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::DeployHistory {
                app,
                namespace,
                result,
            } => match result {
                Ok(value) => {
                    self.data
                        .deploy_history
                        .insert(super::state::deploy_history_key(&app, &namespace), value);
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::AppMetrics(result) => match result {
                Ok(value) => {
                    self.data.app_metrics = Some(value);
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::DesiredApps(result) => set_data!(result, desired_apps),
            DataUpdate::Scaled { app, to, result } => self.scaled(&app, to, result),
            DataUpdate::InstanceKilled { target, result } => self.killed(target, result),
        }
    }

    /// Say how a kill went. A refusal isn't a lost connection: the API
    /// answered, so its words go in the status line as they are.
    fn killed(
        &mut self,
        target: InstanceTarget,
        result: Result<crate::smoker::types::FaultSummary, super::data::ProviderError>,
    ) {
        use super::data::ProviderError;
        self.status_message = Some(match result {
            Ok(fault) => {
                self.pending.push(Effect::RefreshAll);
                (
                    format!(
                        "killed {} on {} (fault {}); the scheduler starts a replacement",
                        target.instance, target.node, fault.id
                    ),
                    Level::Info,
                )
            }
            Err(ProviderError::Api { body, .. }) => {
                (format!("kill refused: {}", body.trim()), Level::Error)
            }
            Err(error) => (format!("kill failed: {error}"), Level::Error),
        });
    }

    fn push_log_line(&mut self, line: LogLine) {
        if self.log_lines.len() == 10_000 {
            self.log_lines.pop_front();
        }
        self.log_lines.push_back(line);
        if self.log_follow {
            self.log_scroll = 0;
        }
    }

    fn mark_connected(&mut self) {
        self.connection = Connection::Connected;
        self.data.last_updated = Some(self.now_epoch);
    }

    fn mark_disconnected(&mut self, error: String) {
        let since_epoch = match self.connection {
            Connection::Disconnected { since_epoch, .. } => since_epoch,
            Connection::Connected => self.now_epoch,
        };
        self.connection = Connection::Disconnected {
            since_epoch,
            last_error: error,
        };
    }

    fn handle_stream(&mut self, item: StreamItem) {
        match item {
            StreamItem::LogLine(line) => self.push_log_line(line),
            // Shown in line, where it happened, so the gap in that node's
            // lines has its explanation right beside it.
            StreamItem::LogWarning(warning) => self.push_log_line(LogLine {
                instance: "warning".into(),
                line: warning,
            }),
            StreamItem::Event(event) => {
                if self.data.events.len() == 1024 {
                    self.data.events.pop_front();
                }
                self.data.events.push_back(event);
            }
            StreamItem::StreamDown {
                what: StreamKind::Logs,
                error,
            } => self.log_stream_down = Some(error),
            StreamItem::StreamDown {
                what: StreamKind::Events,
                error,
            } => self.event_stream_down = Some(error),
            StreamItem::StreamUp {
                what: StreamKind::Logs,
            } => self.log_stream_down = None,
            StreamItem::StreamUp {
                what: StreamKind::Events,
            } => self.event_stream_down = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::bun::agent::{ClusterInstanceStatus, InstanceStatus};
    use crate::relish::tui::data::ProviderError;
    use crate::relish::tui::fixtures::TestScenario;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn stack_starts_at_dashboard_and_q_quits() {
        let mut app = TuiApp::new();
        assert_eq!(app.view(), &View::Dashboard);
        app.update(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn a_failed_summary_keeps_the_last_one() {
        let mut app = TuiApp::with_test_data(super::super::fixtures::TestScenario::HealthyCluster);
        let before = app.data.summary.clone();
        app.update(Msg::Data(DataUpdate::Summary(Err(
            ProviderError::Unreachable("gone".into()),
        ))));
        assert_eq!(app.data.summary, before);
        assert!(matches!(app.connection, Connection::Disconnected { .. }));
        let fresh = super::super::fixtures::TestScenario::NodeMissing
            .data()
            .summary;
        app.update(Msg::Data(DataUpdate::Summary(Ok(Box::new(
            fresh.clone().unwrap(),
        )))));
        assert_eq!(app.data.summary, fresh);
        assert!(matches!(app.connection, Connection::Connected));
    }

    #[test]
    fn error_preserves_stale_data() {
        let mut app = TuiApp::new();
        app.data.instances.push(ClusterInstanceStatus {
            node: "node-1".into(),
            instance: InstanceStatus {
                id: "one".into(),
                app_name: "web".into(),
                namespace: "default".into(),
                state: "running".into(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: None,
                image: None,
                data_on_node: false,
                sidecars: Vec::new(),
                reason: None,
            },
        });
        app.update(Msg::Data(DataUpdate::Status(Err(
            ProviderError::Unreachable("gone".into()),
        ))));
        assert_eq!(app.data.instances.len(), 1);
        assert!(matches!(app.connection, Connection::Disconnected { .. }));
    }

    #[test]
    fn changing_log_stream_discards_queued_lines_and_errors_from_the_previous_target() {
        let mut app = TuiApp::new();
        let old = app.reset_log_stream();
        app.update(Msg::LogStream {
            generation: old,
            item: StreamItem::LogLine(LogLine {
                instance: "web".into(),
                line: "old namespace".into(),
            }),
        });
        assert_eq!(app.log_lines.len(), 1);
        let current = app.reset_log_stream();
        app.update(Msg::LogStream {
            generation: old,
            item: StreamItem::LogLine(LogLine {
                instance: "web".into(),
                line: "late old namespace".into(),
            }),
        });
        app.update(Msg::LogStream {
            generation: old,
            item: StreamItem::StreamDown {
                what: StreamKind::Logs,
                error: "stale disconnect".into(),
            },
        });
        assert!(app.log_lines.is_empty());
        assert!(app.log_stream_down.is_none());
        app.update(Msg::LogStream {
            generation: current,
            item: StreamItem::LogLine(LogLine {
                instance: "web".into(),
                line: "new namespace".into(),
            }),
        });
        assert_eq!(app.log_lines[0].line, "new namespace");
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut app = TuiApp::new();
        for number in 0..10_001 {
            app.update(Msg::Stream(StreamItem::LogLine(LogLine {
                instance: "web".into(),
                line: number.to_string(),
            })));
        }
        assert_eq!(app.log_lines.len(), 10_000);
        assert_eq!(
            app.log_lines.front().map(|line| line.line.as_str()),
            Some("1")
        );
    }

    /// F07 part 2: two nodes ran the same job. The detail page must show
    /// the row that was selected, not the first job with that name.
    #[test]
    fn job_detail_shows_the_selected_nodes_instance() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        let mut second = app.data.jobs[0].clone();
        second.node = "node-2".into();
        second.row.state = "failed".into();
        app.data.jobs.push(second);
        app.view_stack.push(View::Jobs);
        app.jobs_cursor = 1;
        app.update(key(KeyCode::Enter));

        let text = crate::relish::tui::fixtures::render_to_string(&app, 120, 40);
        assert!(text.contains("node-2"), "{text}");
        assert!(text.contains("failed"), "{text}");
    }

    #[test]
    fn navigation_drills_in_and_tabs_wrap() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.update(key(KeyCode::Char('a')));
        app.update(key(KeyCode::Enter));
        assert!(matches!(
            app.view(),
            View::AppDetail {
                tab: DetailTab::Overview,
                ..
            }
        ));
        for _ in 0..6 {
            app.update(key(KeyCode::Tab));
        }
        assert!(matches!(
            app.view(),
            View::AppDetail {
                tab: DetailTab::Overview,
                ..
            }
        ));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.view(), &View::Apps);
    }

    // The arrow keys switch the app detail tabs, as Tab and Shift-Tab do
    // (#2169).
    #[test]
    fn left_and_right_switch_app_detail_tabs() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Overview,
        });
        let tab = |app: &TuiApp| match app.view() {
            View::AppDetail { tab, .. } => *tab,
            other => panic!("left the app detail view for {other:?}"),
        };
        app.update(key(KeyCode::Right));
        assert_eq!(tab(&app), DetailTab::Instances);
        app.update(key(KeyCode::Right));
        assert_eq!(tab(&app), DetailTab::Logs);
        app.update(key(KeyCode::Left));
        assert_eq!(tab(&app), DetailTab::Instances);
        app.update(key(KeyCode::Left));
        app.update(key(KeyCode::Left));
        // Wraps round, as Shift-Tab does.
        assert_eq!(tab(&app), DetailTab::Config);
    }

    // Enter on a node opens its page; the arrows and Tab switch its tabs,
    // and Esc goes back to the list (#2170).
    #[test]
    fn enter_opens_a_node_and_the_arrows_switch_its_tabs() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.update(key(KeyCode::Char('n')));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Enter));
        let tab = |app: &TuiApp| match app.view() {
            View::NodeDetail { node, tab } => {
                assert_eq!(node, "node-2");
                *tab
            }
            other => panic!("left the node page for {other:?}"),
        };
        assert_eq!(tab(&app), NodeTab::Overview);
        app.update(key(KeyCode::Right));
        assert_eq!(tab(&app), NodeTab::Workloads);
        app.update(key(KeyCode::Tab));
        assert_eq!(tab(&app), NodeTab::Maintenance);
        app.update(key(KeyCode::Right));
        assert_eq!(tab(&app), NodeTab::Overview, "wraps round");
        app.update(key(KeyCode::Left));
        assert_eq!(tab(&app), NodeTab::Maintenance);
        assert!(app.pending.is_empty(), "node tabs fetch nothing");
        app.update(key(KeyCode::Esc));
        assert_eq!(app.view(), &View::Nodes);
    }

    #[test]
    fn enter_on_a_workload_opens_its_app() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::NodeDetail {
            node: "node-2".into(),
            tab: NodeTab::Workloads,
        });
        // node-2 runs app-2-1, web-1 and worker-1, in that order.
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.instances_cursor, 2, "stops at the last row");
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Enter));
        assert_eq!(
            app.view(),
            &View::AppDetail {
                app: "web".into(),
                namespace: "default".into(),
                tab: DetailTab::Overview,
            }
        );
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.view(), View::NodeDetail { .. }));
    }

    #[test]
    fn k_on_the_workloads_tab_does_nothing() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::NodeDetail {
            node: "node-1".into(),
            tab: NodeTab::Workloads,
        });
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.confirm_kill, None, "the node page is read-only");
    }

    #[test]
    fn dashboard_r_opens_routes_while_list_r_refreshes() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.update(key(KeyCode::Char('r')));
        assert_eq!(app.view(), &View::Routes);

        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Apps);
        app.update(key(KeyCode::Char('r')));
        assert_eq!(app.pending.last(), Some(&Effect::RefreshAll));
    }

    #[test]
    fn entering_logs_tab_requests_a_stream() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Instances,
        });
        app.update(key(KeyCode::Tab));
        assert_eq!(
            app.pending.last(),
            Some(&Effect::OpenLogStream {
                app: "web".into(),
                namespace: "default".into()
            })
        );
    }

    #[test]
    fn palette_resolves_log_namespace_from_cached_status() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.update(key(KeyCode::Char(':')));
        for character in "logs web".chars() {
            app.update(key(KeyCode::Char(character)));
        }
        app.update(key(KeyCode::Enter));
        assert_eq!(
            app.view(),
            &View::Logs {
                app: Some(("web".into(), "default".into()))
            }
        );
    }

    fn type_text(app: &mut TuiApp, text: &str) {
        for character in text.chars() {
            app.update(key(KeyCode::Char(character)));
        }
    }

    fn app_detail(tab: DetailTab) -> TuiApp {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab,
        });
        app
    }

    // `/` searches the Logs tab; Esc clears the search and stays put, and
    // only a second Esc goes back (#2174).
    #[test]
    fn slash_searches_the_logs_tab_and_esc_clears_it_without_leaving() {
        let mut app = app_detail(DetailTab::Logs);
        app.update(key(KeyCode::Char('/')));
        assert!(app.filter_editing);
        type_text(&mut app, "err");
        assert_eq!(app.filter.as_deref(), Some("err"));
        app.update(key(KeyCode::Enter));
        assert!(!app.filter_editing);
        assert_eq!(app.filter.as_deref(), Some("err"));

        app.update(key(KeyCode::Esc));
        assert_eq!(app.filter, None);
        assert!(matches!(
            app.view(),
            View::AppDetail {
                tab: DetailTab::Logs,
                ..
            }
        ));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.view(), &View::Dashboard);
    }

    #[test]
    fn esc_while_typing_a_search_clears_it() {
        let mut app = app_detail(DetailTab::Metrics);
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "cpu");
        app.update(key(KeyCode::Esc));
        assert_eq!(app.filter, None);
        assert!(!app.filter_editing);
        assert!(matches!(
            app.view(),
            View::AppDetail {
                tab: DetailTab::Metrics,
                ..
            }
        ));
    }

    // While the search box is open, letters are search text, not keys:
    // `l` and `q` don't open the logs view or go back.
    #[test]
    fn typing_in_the_logs_view_search_box_does_not_navigate() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Logs {
            app: Some(("web".into(), "default".into())),
        });
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "slow query");
        assert_eq!(app.filter.as_deref(), Some("slow query"));
        assert!(matches!(app.view(), View::Logs { app: Some(_) }));
        app.update(key(KeyCode::Backspace));
        assert_eq!(app.filter.as_deref(), Some("slow quer"));
    }

    // A search belongs to the screen it was typed on: another tab or view
    // starts without it.
    #[test]
    fn a_search_does_not_follow_you_to_another_tab_or_view() {
        let mut app = app_detail(DetailTab::Logs);
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "err");
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.filter, None);

        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "cpu");
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('q')));
        assert_eq!(app.view(), &View::Dashboard);
        assert_eq!(app.filter, None);

        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "web");
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('e')));
        assert_eq!(app.filter, None);
    }

    #[test]
    fn visible_log_lines_keep_only_matching_lines_ignoring_case() {
        let mut app = app_detail(DetailTab::Logs);
        for line in [
            "GET /health 200",
            "ERROR: disk full",
            "retrying after error",
        ] {
            app.log_lines.push_back(LogLine {
                instance: "web-0".into(),
                line: line.into(),
            });
        }
        assert_eq!(app.visible_log_lines().len(), 3);
        app.filter = Some("error".into());
        let visible: Vec<_> = app
            .visible_log_lines()
            .iter()
            .map(|line| line.line.as_str())
            .collect();
        assert_eq!(visible, ["ERROR: disk full", "retrying after error"]);
        app.filter = Some("panic".into());
        assert!(app.visible_log_lines().is_empty());
    }

    fn web_1_on_node_2() -> InstanceTarget {
        InstanceTarget {
            app: "web".into(),
            namespace: "default".into(),
            instance: "web-1".into(),
            node: "node-2".into(),
        }
    }

    fn kills(app: &TuiApp) -> Vec<&Effect> {
        app.pending
            .iter()
            .filter(|effect| matches!(effect, Effect::KillInstance(_)))
            .collect()
    }

    // `k` asks first, and anything but `y` sends nothing (#2175).
    #[test]
    fn k_without_confirming_does_nothing() {
        for answer in [
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('k'),
        ] {
            let mut app = app_detail(DetailTab::Instances);
            app.update(key(KeyCode::Char('k')));
            assert!(app.confirm_kill.is_some(), "k asks before it kills");
            assert!(kills(&app).is_empty(), "k alone sent a kill");
            app.update(key(answer));
            assert_eq!(app.confirm_kill, None, "{answer:?} left the prompt open");
            assert!(kills(&app).is_empty(), "{answer:?} sent a kill");
            assert!(!app.should_quit, "{answer:?} quit instead of cancelling");
            assert!(matches!(
                app.view(),
                View::AppDetail {
                    tab: DetailTab::Instances,
                    ..
                }
            ));
            assert_eq!(
                app.status_message,
                Some(("kill cancelled".into(), Level::Info))
            );
        }
    }

    #[test]
    fn y_kills_the_selected_instance_on_its_node() {
        let mut app = app_detail(DetailTab::Instances);
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.confirm_kill, Some(web_1_on_node_2()));
        app.update(key(KeyCode::Char('y')));
        assert_eq!(app.confirm_kill, None);
        assert_eq!(kills(&app), [&Effect::KillInstance(web_1_on_node_2())]);
    }

    #[test]
    fn ctrl_k_asks_as_k_does() {
        let mut app = app_detail(DetailTab::Instances);
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(
            app.confirm_kill
                .as_ref()
                .map(|target| target.instance.as_str()),
            Some("web-0")
        );
    }

    #[test]
    fn up_and_down_select_an_instance_and_stop_at_either_end() {
        let mut app = app_detail(DetailTab::Instances);
        assert_eq!(app.instances_cursor, 0);
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.instances_cursor, 1, "web has two instances");
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Up));
        assert_eq!(app.instances_cursor, 0);
    }

    #[test]
    fn k_outside_the_instances_tab_does_nothing() {
        for tab in [DetailTab::Overview, DetailTab::Logs, DetailTab::Metrics] {
            let mut app = app_detail(tab);
            app.update(key(KeyCode::Char('k')));
            assert_eq!(app.confirm_kill, None, "{tab:?}");
        }
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Apps);
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.confirm_kill, None);
    }

    #[test]
    fn a_kill_says_what_it_did_and_refreshes() {
        let mut app = app_detail(DetailTab::Instances);
        app.update(Msg::Data(DataUpdate::InstanceKilled {
            target: web_1_on_node_2(),
            result: Ok(crate::smoker::types::FaultSummary {
                id: 7,
                fault_type: "kill".into(),
                target_service: "web".into(),
                namespace: Some("default".into()),
                target_instance: Some("web-1".into()),
                task_index: None,
                target_node: Some("node-2".into()),
                remaining_secs: 0,
                injected_by: "admin".into(),
                node: Some("node-2".into()),
                routed: Vec::new(),
            }),
        }));
        let (message, level) = app.status_message.clone().unwrap();
        assert_eq!(level, Level::Info);
        assert!(message.contains("killed web-1 on node-2"), "{message}");
        assert!(message.contains("fault 7"), "{message}");
        assert_eq!(app.pending.last(), Some(&Effect::RefreshAll));
    }

    // A refusal (a scoped token, a safety rail) shows the API's own words.
    #[test]
    fn a_refused_kill_shows_the_refusal() {
        let mut app = app_detail(DetailTab::Instances);
        app.update(Msg::Data(DataUpdate::InstanceKilled {
            target: web_1_on_node_2(),
            result: Err(ProviderError::Api {
                status: 403,
                body: "token scope does not cover namespace default\n".into(),
            }),
        }));
        assert_eq!(
            app.status_message,
            Some((
                "kill refused: token scope does not cover namespace default".into(),
                Level::Error
            ))
        );
        assert!(matches!(app.connection, Connection::Connected));
        assert!(app.pending.is_empty());
    }

    #[test]
    fn search_matches_across_cached_resources() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.update(key(KeyCode::Char('s')));
        for character in "web".chars() {
            app.update(key(KeyCode::Char(character)));
        }
        assert!(app.search.results.iter().any(|hit| hit.name == "web"));
        assert!(app.search.results.iter().all(|hit| hit.name != "worker"));
    }
}
