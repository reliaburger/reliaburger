//! View-stack navigation and cursor movement.

use super::keys::KeyAction;
use super::state::{DetailTab, Effect, SearchState, TuiApp, View};

impl TuiApp {
    pub(super) fn handle_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::Apps => self.push(View::Apps),
            KeyAction::Nodes => self.push(View::Nodes),
            KeyAction::Jobs => self.push(View::Jobs),
            KeyAction::Events => self.push(View::Events),
            KeyAction::Logs => self.push(View::Logs { app: None }),
            KeyAction::Routes => self.push(View::Routes),
            KeyAction::Search => {
                self.search = SearchState::default();
                self.push(View::Search);
            }
            KeyAction::Help => self.push(View::Help),
            KeyAction::Palette => self.palette = Some(Default::default()),
            KeyAction::Back => self.back(),
            KeyAction::Enter => self.open_selection(),
            KeyAction::Up => self.move_cursor(-1),
            KeyAction::Down => self.move_cursor(1),
            KeyAction::PageUp => self.scroll(10),
            KeyAction::PageDown => self.scroll(-10),
            KeyAction::Home => {
                self.events_scroll = usize::MAX;
                self.log_scroll = usize::MAX;
            }
            KeyAction::End => {
                self.events_scroll = 0;
                self.log_scroll = 0;
                self.log_follow = true;
            }
            KeyAction::Filter => {
                self.filter = Some(String::new());
                self.filter_editing = true;
            }
            KeyAction::Refresh => self.pending.push(Effect::RefreshAll),
            KeyAction::ToggleFollow => self.log_follow = !self.log_follow,
            KeyAction::NextTab => self.cycle_tab(1),
            KeyAction::PreviousTab => self.cycle_tab(-1),
        }
    }

    pub(super) fn push(&mut self, view: View) {
        if self.view() != &view {
            self.view_stack.push(view);
        }
    }

    pub(super) fn back(&mut self) {
        if self.view_stack.len() == 1 {
            self.should_quit = true;
        } else {
            if self.view_is_log_stream() {
                self.pending.push(Effect::CloseLogStream);
            }
            self.view_stack.pop();
        }
    }

    fn view_is_log_stream(&self) -> bool {
        matches!(
            self.view(),
            View::Logs { app: Some(_) }
                | View::AppDetail {
                    tab: DetailTab::Logs,
                    ..
                }
        )
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some(View::AppDetail {
            app,
            namespace,
            tab,
        }) = self.view_stack.last_mut()
        else {
            return;
        };
        let old = *tab;
        *tab = tab.offset(delta);
        if old == DetailTab::Logs {
            self.pending.push(Effect::CloseLogStream);
        }
        match *tab {
            DetailTab::Logs => self.pending.push(Effect::OpenLogStream {
                app: app.clone(),
                namespace: namespace.clone(),
            }),
            DetailTab::Metrics => self.pending.push(Effect::FetchAppMetrics {
                app: app.clone(),
                namespace: namespace.clone(),
            }),
            DetailTab::Deploys => self.pending.push(Effect::FetchDeployHistory {
                app: app.clone(),
                namespace: namespace.clone(),
            }),
            _ => {}
        }
    }

    fn open_selection(&mut self) {
        match self.view().clone() {
            View::Dashboard | View::Apps => {
                if let Some((app, namespace)) = self.filtered_apps().get(self.apps_cursor).cloned()
                {
                    self.push(View::AppDetail {
                        app,
                        namespace,
                        tab: DetailTab::Overview,
                    });
                }
            }
            View::Nodes => {
                if let Some(node) = self.data.nodes.get(self.nodes_cursor) {
                    self.push(View::NodeDetail {
                        node: node.node_id.clone(),
                    });
                }
            }
            View::Jobs => {
                if let Some(job) = self.data.jobs.get(self.jobs_cursor) {
                    self.push(View::JobDetail {
                        name: job.name.clone(),
                        namespace: job.namespace.clone(),
                    });
                }
            }
            View::Routes => {
                if let Some(route) = self.data.routes.get(self.routes_cursor) {
                    self.push(View::RouteDetail {
                        host: route.host.clone(),
                    });
                }
            }
            View::Logs { app: None } => {
                if let Some((app, namespace)) = self.filtered_apps().get(self.apps_cursor).cloned()
                {
                    self.view_stack.pop();
                    self.push(View::Logs {
                        app: Some((app.clone(), namespace.clone())),
                    });
                    self.pending.push(Effect::OpenLogStream { app, namespace });
                }
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        fn moved(cursor: usize, len: usize, delta: isize) -> usize {
            if len == 0 {
                0
            } else {
                (cursor as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize
            }
        }
        match self.view() {
            View::Dashboard | View::Apps | View::Logs { app: None } => {
                self.apps_cursor = moved(self.apps_cursor, self.filtered_apps().len(), delta)
            }
            View::Nodes => {
                self.nodes_cursor = moved(self.nodes_cursor, self.data.nodes.len(), delta)
            }
            View::Jobs => self.jobs_cursor = moved(self.jobs_cursor, self.data.jobs.len(), delta),
            View::Routes => {
                self.routes_cursor = moved(self.routes_cursor, self.data.routes.len(), delta)
            }
            View::Help => {
                self.events_scroll =
                    moved(self.events_scroll, super::keys::KEYBINDINGS.len(), delta)
            }
            _ => {}
        }
    }

    fn scroll(&mut self, delta: isize) {
        if matches!(self.view(), View::Events) {
            self.events_scroll = self.events_scroll.saturating_add_signed(delta);
        } else {
            self.log_follow = false;
            self.log_scroll = self.log_scroll.saturating_add_signed(delta);
        }
    }
}
