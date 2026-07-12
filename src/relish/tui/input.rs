//! Keyboard input modes, palette dispatch, and in-memory search.

use crossterm::event::{KeyCode, KeyEvent};

use super::keys::key_to_action;
use super::palette::{PaletteCommand, parse_command};
use super::state::{DetailTab, Effect, Level, SearchHit, TuiApp, View};

impl TuiApp {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if self.palette.is_some() {
            self.handle_palette_key(key);
        } else if matches!(self.view(), View::Search) {
            self.handle_search_key(key);
        } else if self.filter_editing {
            self.handle_filter_key(key);
        } else if let Some(action) = key_to_action(key) {
            let action = if action == super::keys::KeyAction::Refresh
                && matches!(
                    self.view(),
                    View::Dashboard
                        | View::AppDetail { .. }
                        | View::NodeDetail { .. }
                        | View::JobDetail { .. }
                        | View::Logs { .. }
                        | View::Help
                ) {
                super::keys::KeyAction::Routes
            } else {
                action
            };
            self.handle_action(action);
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let filter = self.filter.get_or_insert_default();
        match key.code {
            KeyCode::Esc => {
                self.filter = None;
                self.filter_editing = false;
            }
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Backspace => {
                filter.pop();
            }
            KeyCode::Char(character) => filter.push(character),
            _ => {}
        }
        self.apps_cursor = 0;
    }

    fn handle_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Backspace => {
                if let Some(palette) = &mut self.palette {
                    palette.input.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(palette) = &mut self.palette {
                    palette.input.push(character);
                }
            }
            KeyCode::Enter => {
                let input = self
                    .palette
                    .take()
                    .map(|palette| palette.input)
                    .unwrap_or_default();
                match parse_command(&input) {
                    Ok(command) => self.run_palette(command),
                    Err(error) => self.status_message = Some((error, Level::Error)),
                }
            }
            _ => {}
        }
    }

    fn run_palette(&mut self, command: PaletteCommand) {
        match command {
            PaletteCommand::Apps => self.push(View::Apps),
            PaletteCommand::Nodes => self.push(View::Nodes),
            PaletteCommand::Jobs => self.push(View::Jobs),
            PaletteCommand::Events => self.push(View::Events),
            PaletteCommand::Routes => self.push(View::Routes),
            PaletteCommand::Search => self.push(View::Search),
            PaletteCommand::Help => self.push(View::Help),
            PaletteCommand::Quit => self.should_quit = true,
            PaletteCommand::Logs(None) => self.push(View::Logs { app: None }),
            PaletteCommand::Logs(Some(name)) => {
                let namespace = self
                    .data
                    .instances
                    .iter()
                    .find(|instance| instance.app_name == name)
                    .map(|instance| instance.namespace.clone())
                    .unwrap_or_else(|| "default".to_string());
                self.push(View::Logs {
                    app: Some((name.clone(), namespace.clone())),
                });
                self.pending.push(Effect::OpenLogStream {
                    app: name,
                    namespace,
                });
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.back(),
            KeyCode::Enter => {
                if let Some(hit) = self.search.results.get(self.search.cursor) {
                    self.push(hit.target.clone());
                }
            }
            KeyCode::Up => self.search.cursor = self.search.cursor.saturating_sub(1),
            KeyCode::Down => {
                self.search.cursor =
                    (self.search.cursor + 1).min(self.search.results.len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.search.input.pop();
                self.rebuild_search();
            }
            KeyCode::Char(character) => {
                self.search.input.push(character);
                self.rebuild_search();
            }
            _ => {}
        }
    }

    fn rebuild_search(&mut self) {
        let query = self.search.input.to_lowercase();
        let mut hits = Vec::new();
        if !query.is_empty() {
            for (app, namespace) in self.all_apps() {
                if app.to_lowercase().contains(&query) {
                    hits.push(SearchHit {
                        kind: "app".into(),
                        name: app.clone(),
                        context: namespace.clone(),
                        target: View::AppDetail {
                            app,
                            namespace,
                            tab: DetailTab::Overview,
                        },
                    });
                }
            }
            for node in &self.data.nodes {
                if node.node_id.to_lowercase().contains(&query) {
                    hits.push(SearchHit {
                        kind: "node".into(),
                        name: node.node_id.clone(),
                        context: node.address.clone(),
                        target: View::NodeDetail {
                            node: node.node_id.clone(),
                        },
                    });
                }
            }
            for job in &self.data.jobs {
                if job.name.to_lowercase().contains(&query) {
                    hits.push(SearchHit {
                        kind: "job".into(),
                        name: job.name.clone(),
                        context: job.namespace.clone(),
                        target: View::JobDetail {
                            name: job.name.clone(),
                            namespace: job.namespace.clone(),
                        },
                    });
                }
            }
            for route in &self.data.routes {
                if route.host.to_lowercase().contains(&query)
                    || route.app_name.to_lowercase().contains(&query)
                {
                    hits.push(SearchHit {
                        kind: "route".into(),
                        name: route.host.clone(),
                        context: route.app_name.clone(),
                        target: View::RouteDetail {
                            host: route.host.clone(),
                        },
                    });
                }
            }
            for event in &self.data.events {
                if event.message.to_lowercase().contains(&query) {
                    hits.push(SearchHit {
                        kind: "event".into(),
                        name: event.message.clone(),
                        context: event.app.clone().unwrap_or_else(|| "cluster".into()),
                        target: View::Events,
                    });
                }
            }
        }
        hits.sort_by_key(|hit| !hit.name.to_lowercase().starts_with(&query));
        self.search.results = hits;
        self.search.cursor = 0;
    }
}
