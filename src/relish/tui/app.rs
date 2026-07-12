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
        }
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
            DataUpdate::Jobs(result) => set_data!(result, jobs),
            DataUpdate::EventsSeed(result) => match result {
                Ok(value) => {
                    self.data.events = value.into();
                    self.mark_connected();
                }
                Err(error) => self.mark_disconnected(error.to_string()),
            },
            DataUpdate::DeployHistory { app, result } => match result {
                Ok(value) => {
                    self.data.deploy_history.insert(app, value);
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
            StreamItem::LogLine(line) => {
                if self.log_lines.len() == 10_000 {
                    self.log_lines.pop_front();
                }
                self.log_lines.push_back(line);
                if self.log_follow {
                    self.log_scroll = 0;
                }
            }
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
    use crate::bun::agent::InstanceStatus;
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
    fn error_preserves_stale_data() {
        let mut app = TuiApp::new();
        app.data.instances.push(InstanceStatus {
            id: "one".into(),
            app_name: "web".into(),
            namespace: "default".into(),
            state: "running".into(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: None,
        });
        app.update(Msg::Data(DataUpdate::Status(Err(
            ProviderError::Unreachable("gone".into()),
        ))));
        assert_eq!(app.data.instances.len(), 1);
        assert!(matches!(app.connection, Connection::Disconnected { .. }));
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
