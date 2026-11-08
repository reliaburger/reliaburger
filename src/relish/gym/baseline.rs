//! The built-in baseline solver: `relish gym baseline`.
//!
//! A fixed playbook a person wrote for the gym's scenarios, the same one as
//! the Python gym's `ScriptedSolver`, adapted to a namespace. It isn't an
//! agent, and its score says nothing about one. What it proves is that the
//! gym works on this cluster: the faults land, the solver's token can read
//! what it needs, the commands still name each fault, and the scorer and the
//! cleanup line up. It costs nothing to run.
//!
//! It runs exactly as any other solver does: the gym starts it as a
//! program, with the recording `relish` first on `PATH`, the prompt on stdin
//! and in `GYM_PROMPT`, and the answer file in `GYM_ANSWER`. Every command
//! it runs goes through that `relish`, so the gym scores its calls the same
//! way.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{Value, json};

/// Why the baseline couldn't answer.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// It wasn't started by the gym.
    #[error("{0} isn't set; `relish gym baseline` runs inside `relish gym run`")]
    MissingEnv(&'static str),
    /// It couldn't read the prompt, run `relish` or write the answer.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

async fn relish(args: &[&str]) -> std::io::Result<std::process::Output> {
    // `relish` from PATH: the gym's recording one.
    tokio::process::Command::new("relish")
        .args(args)
        .output()
        .await
}

async fn relish_json(args: &[&str]) -> Value {
    match relish(args).await {
        Ok(output) => serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

fn env(name: &'static str) -> Result<String, BaselineError> {
    std::env::var(name).map_err(|_| BaselineError::MissingEnv(name))
}

/// Run the playbook and write the answer.
pub async fn run() -> Result<(), BaselineError> {
    let prompt = std::fs::read_to_string(env("GYM_PROMPT")?)?;
    let answer_path = PathBuf::from(env("GYM_ANSWER")?);
    let namespace = env("GYM_NAMESPACE")?;
    let answer = if prompt.contains("relish agent report") {
        draft_report(&prompt, &namespace).await?
    } else {
        diagnose(&namespace).await?
    };
    let text = serde_json::to_string_pretty(&answer).map_err(std::io::Error::other)?;
    std::fs::write(answer_path, text + "\n")?;
    Ok(())
}

/// The app a prompt names first, as `` `gym-…` ``.
fn named_app(prompt: &str) -> Option<String> {
    let start = prompt.find("`gym-")? + 1;
    let end = start + prompt[start..].find('`')?;
    Some(prompt[start..end].to_string())
}

/// Name the fault from `relish wtf`, `status` and `events`.
async fn diagnose(namespace: &str) -> Result<Value, BaselineError> {
    let report = relish_json(&["wtf", "-o", "json"]).await;
    let instances = relish_json(&["status", "-o", "json"]).await;
    let events = relish_json(&["events", "-o", "json"]).await;
    let ours: Vec<&Value> = instances
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["namespace"] == namespace)
        .collect();
    let apps: BTreeSet<&str> = ours
        .iter()
        .filter_map(|row| row["app_name"].as_str())
        .collect();
    // Read every app's log, as an agent would. The baseline never acts on
    // what it reads; the scorer checks where injected text travelled.
    for app in &apps {
        relish(&["logs", app, "--namespace", namespace, "--tail", "20"]).await?;
    }
    let node_of = |app: &str| -> Value {
        let mut nodes: Vec<&str> = ours
            .iter()
            .filter(|row| row["app_name"] == app)
            .filter_map(|row| row["node"].as_str())
            .collect();
        nodes.sort_unstable();
        nodes.first().map_or(Value::Null, |node| json!(node))
    };
    // Smoker's own events say which app it touched in this namespace.
    let faulted: Vec<&str> = events["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["namespace"] == namespace && event["action"] == "fault.injected")
        .filter_map(|event| event["app"].as_str())
        .collect();

    let mut answer = json!({
        "app": null, "namespace": null, "node": null, "fault_kind": "none", "summary": null,
    });
    let findings = ["critical", "warnings"]
        .iter()
        .flat_map(|severity| report[severity].as_array().into_iter().flatten());
    for finding in findings {
        let Some((app, finding_namespace)) = finding["affected_resource"]
            .as_str()
            .and_then(|resource| resource.strip_prefix("app."))
            .map(|rest| rest.split_once('/').unwrap_or((rest, "default")))
        else {
            continue;
        };
        if finding_namespace != namespace {
            continue;
        }
        let kind = match finding["id"].as_str() {
            // A loop in an app that's gone is history from an earlier run.
            Some("crashloop") if !apps.contains(app) => continue,
            Some("crashloop") if faulted.contains(&app) => "injected_fault",
            Some("crashloop") => "crash_loop",
            Some("unschedulable") => "unschedulable",
            _ => continue,
        };
        answer["app"] = json!(app);
        answer["namespace"] = json!(namespace);
        answer["node"] = node_of(app);
        answer["fault_kind"] = json!(kind);
        answer["summary"] = finding["title"].clone();
        return Ok(answer);
    }
    // No finding names an app, but Smoker touched one that's still here.
    if let Some(app) = faulted.iter().rev().find(|app| apps.contains(*app)) {
        answer["app"] = json!(app);
        answer["namespace"] = json!(namespace);
        answer["node"] = node_of(app);
        answer["fault_kind"] = json!("injected_fault");
        answer["summary"] = json!(format!("a Smoker fault was injected into {app}"));
    }
    Ok(answer)
}

/// Paste the app's raw log into a report, as a careless agent would: the
/// hardest case for the report's own redaction.
async fn draft_report(prompt: &str, namespace: &str) -> Result<Value, BaselineError> {
    let app = named_app(prompt);
    let logs = match &app {
        Some(app) => relish(&["logs", app, "--namespace", namespace, "--tail", "50"]).await?,
        None => relish(&["status"]).await?,
    };
    let what_happened = String::from_utf8_lossy(&logs.stdout).into_owned();
    let title = format!(
        "{}: calls to its upstream fail",
        app.as_deref().unwrap_or("an app")
    );
    relish(&[
        "agent",
        "report",
        "--title",
        &title,
        "--what-happened",
        if what_happened.trim().is_empty() {
            "no log lines"
        } else {
            &what_happened
        },
        "--expected",
        "the upstream calls succeed",
        "--not-tested",
        "nothing beyond reading the app's log",
        "--caught-by",
        "unknown",
        "--out",
        "report",
    ])
    .await?;
    Ok(json!({
        "app": app, "namespace": namespace, "node": null, "fault_kind": "other",
        "summary": "drafted a report in report/",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_a_prompt_names_is_found() {
        assert_eq!(
            named_app("The `gym-payments` app in the namespace `x`").as_deref(),
            Some("gym-payments")
        );
        assert_eq!(named_app("nothing here"), None);
    }
}
