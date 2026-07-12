use ratatui::text::Line;

use crate::relish::tui::app::{TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::JobDetail { name, namespace } => {
            let Some(job) = app
                .data
                .jobs
                .iter()
                .find(|job| &job.name == name && &job.namespace == namespace)
            else {
                return vec![Line::raw("job not found")];
            };
            vec![
                widgets::heading(&job.name),
                Line::raw(format!("namespace       {}", job.namespace)),
                Line::raw(format!("instance        {}", job.instance_id)),
                Line::raw(format!("state           {}", job.state)),
                Line::raw(format!("restarts        {}", job.restart_count)),
                Line::raw(format!("age             {}s", job.age_seconds)),
                Line::raw(format!("image           {}", job.image)),
            ]
        }
        _ => {
            let mut lines = vec![widgets::heading(
                "NAME                 NS           STATE        RESTARTS  AGE      IMAGE",
            )];
            if app.data.jobs.is_empty() {
                lines.push(Line::raw("no jobs"));
            }
            for (index, job) in app.data.jobs.iter().enumerate() {
                lines.push(widgets::row(
                    format!(
                        "{:<20} {:<12} {:<12} {:>8}  {:>6}s  {}",
                        job.name,
                        job.namespace,
                        job.state,
                        job.restart_count,
                        job.age_seconds,
                        job.image
                    ),
                    index == app.jobs_cursor,
                ));
            }
            lines
        }
    }
}
