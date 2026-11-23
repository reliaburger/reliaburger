//! Scaling an app from its Instances tab (#2176): `+` or `-` asks, `y`
//! answers, and the reducer hands the event loop an [`Effect::Scale`].
//!
//! The count it starts from is the one the app's spec asks for, from the
//! council's desired apps, not how many instances happen to run. The
//! request carries that count as a precondition, so a scale someone else
//! made in the meantime isn't overwritten.

use crossterm::event::{KeyCode, KeyEvent};

use super::data::ProviderError;
use super::state::{DetailTab, Effect, Level, ScalePrompt, TuiApp, View};

impl TuiApp {
    /// Handle `+` or `-` on an app's Instances tab. Returns `false` for any
    /// other key or view, which then means what it always does.
    pub(super) fn handle_scale_key(&mut self, key: KeyEvent) -> bool {
        let delta: i64 = match key.code {
            KeyCode::Char('+') | KeyCode::Char('=') => 1,
            KeyCode::Char('-') => -1,
            _ => return false,
        };
        let View::AppDetail {
            app,
            namespace,
            tab: DetailTab::Instances,
        } = self.view()
        else {
            return false;
        };
        let (app, namespace) = (app.clone(), namespace.clone());
        let Some(from) = self.desired_replicas(&app, &namespace) else {
            self.status_message = Some((
                format!("{app}'s replica count isn't known yet; try again in a moment"),
                Level::Warning,
            ));
            return true;
        };
        let to = i64::from(from) + delta;
        if to < 1 {
            self.status_message = Some((
                format!(
                    "{app} runs at least 1 replica; `relish stop {app}` scales it to zero and \
                     keeps its count"
                ),
                Level::Warning,
            ));
            return true;
        }
        let to = u32::try_from(to).unwrap_or(u32::MAX);
        self.scale_prompt = Some(ScalePrompt {
            app,
            namespace,
            from,
            to,
        });
        true
    }

    /// Answer the open scale question: `y` scales, anything else doesn't.
    pub(super) fn answer_scale_prompt(&mut self, key: KeyEvent) {
        let Some(prompt) = self.scale_prompt.take() else {
            return;
        };
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.status_message = Some((
                format!("scaling {} to {}…", prompt.app, prompt.to),
                Level::Info,
            ));
            self.pending.push(Effect::Scale {
                app: prompt.app,
                namespace: prompt.namespace,
                from: prompt.from,
                to: prompt.to,
            });
        } else {
            self.status_message = Some((format!("{} not scaled", prompt.app), Level::Info));
        }
    }

    /// The cluster's answer to a scale: the new count, or why not.
    pub(super) fn scaled(&mut self, app: &str, to: u32, result: Result<(), ProviderError>) {
        match result {
            Ok(()) => {
                self.status_message = Some((
                    format!(
                        "scaled {app} to {to}; applying its file again puts back the file's count"
                    ),
                    Level::Info,
                ));
                self.pending.push(Effect::RefreshAll);
            }
            Err(error) => {
                self.status_message = Some((format!("{app} not scaled: {error}"), Level::Error));
            }
        }
    }

    /// The replica count the app's spec asks for, when the council said.
    pub fn desired_replicas(&self, app: &str, namespace: &str) -> Option<u32> {
        self.data
            .desired_apps
            .iter()
            .find(|desired| desired.app == app && desired.namespace == namespace)
            .map(|desired| desired.desired_replicas)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::fixtures::{TestScenario, render_to_string};
    use super::super::msg::{DataUpdate, Msg};
    use super::*;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn on_instances_tab() -> TuiApp {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Instances,
        });
        app
    }

    #[test]
    fn plus_asks_before_scaling_up_and_y_scales() {
        let mut app = on_instances_tab();
        app.update(key(KeyCode::Char('+')));
        let prompt = app.scale_prompt.clone().expect("a question");
        assert_eq!((prompt.from, prompt.to), (2, 3));
        assert!(app.pending.is_empty(), "nothing happens before the answer");
        let screen = render_to_string(&app, 120, 40);
        assert!(
            screen.contains("Scale web from 2 to 3? Applying its file again puts back"),
            "{screen}"
        );
        insta::assert_snapshot!("scale_prompt", screen);

        app.update(key(KeyCode::Char('y')));
        assert_eq!(app.scale_prompt, None);
        assert_eq!(
            app.pending,
            vec![Effect::Scale {
                app: "web".into(),
                namespace: "default".into(),
                from: 2,
                to: 3,
            }]
        );
    }

    #[test]
    fn minus_asks_to_scale_down_and_any_other_answer_cancels() {
        let mut app = on_instances_tab();
        app.update(key(KeyCode::Char('-')));
        let prompt = app.scale_prompt.clone().expect("a question");
        assert_eq!((prompt.from, prompt.to), (2, 1));
        // `q` would otherwise go back a view; here it only says no.
        app.update(key(KeyCode::Char('q')));
        assert_eq!(app.scale_prompt, None);
        assert!(app.pending.is_empty());
        assert!(matches!(app.view(), View::AppDetail { .. }));
        assert_eq!(
            app.status_message,
            Some(("web not scaled".to_string(), Level::Info))
        );
    }

    #[test]
    fn minus_at_one_points_at_stop_instead_of_asking() {
        let mut app = on_instances_tab();
        app.data.desired_apps[0].desired_replicas = 1;
        app.update(key(KeyCode::Char('-')));
        assert_eq!(app.scale_prompt, None);
        let (message, level) = app.status_message.clone().unwrap();
        assert_eq!(level, Level::Warning);
        assert!(message.contains("relish stop web"), "{message}");
    }

    #[test]
    fn an_unknown_count_asks_nothing() {
        let mut app = on_instances_tab();
        app.data.desired_apps.clear();
        app.update(key(KeyCode::Char('+')));
        assert_eq!(app.scale_prompt, None);
        assert!(app.status_message.unwrap().0.contains("isn't known yet"));
    }

    #[test]
    fn plus_and_minus_do_nothing_outside_the_instances_tab() {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Overview,
        });
        app.update(key(KeyCode::Char('+')));
        app.update(key(KeyCode::Char('-')));
        assert_eq!(app.scale_prompt, None);
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn the_answer_shows_the_new_count_or_why_not() {
        let mut app = on_instances_tab();
        app.update(Msg::Data(DataUpdate::Scaled {
            app: "web".into(),
            to: 3,
            result: Ok(()),
        }));
        let (message, level) = app.status_message.clone().unwrap();
        assert_eq!(level, Level::Info);
        assert!(message.starts_with("scaled web to 3"), "{message}");
        assert_eq!(app.pending, vec![Effect::RefreshAll]);

        app.update(Msg::Data(DataUpdate::Scaled {
            app: "web".into(),
            to: 3,
            result: Err(ProviderError::Api {
                status: 409,
                body: "web is managed by GitOps".into(),
            }),
        }));
        let (message, level) = app.status_message.unwrap();
        assert_eq!(level, Level::Error);
        assert!(message.contains("managed by GitOps"), "{message}");
    }
}
