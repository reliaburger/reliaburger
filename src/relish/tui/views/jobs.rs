use ratatui::text::Line;

use crate::relish::tui::app::{TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::JobDetail {
            name,
            namespace,
            node,
            instance,
        } => {
            let Some(tagged) = app.data.jobs.iter().find(|job| {
                &job.row.name == name
                    && &job.row.namespace == namespace
                    && &job.node == node
                    && &job.row.instance_id == instance
            }) else {
                return vec![Line::raw(format!("job {name} on {node} not found"))];
            };
            let job = &tagged.row;
            vec![
                widgets::heading(&job.name),
                Line::raw(format!("node            {}", tagged.node)),
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
                "NAME                 NODE         NS           STATE        RESTARTS  AGE      IMAGE",
            )];
            lines.extend(widgets::partial_warnings(&app.data.job_warnings));
            if app.data.jobs.is_empty() {
                lines.push(Line::raw("no jobs"));
            }
            for (index, tagged) in app.data.jobs.iter().enumerate() {
                let job = &tagged.row;
                lines.push(widgets::row(
                    format!(
                        "{:<20} {:<12} {:<12} {:<12} {:>8}  {:>6}s  {}",
                        job.name,
                        tagged.node,
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
