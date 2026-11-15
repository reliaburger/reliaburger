//! `relish gym`: test your own agent on your own cluster (#1683).
//!
//! The repository's agent gym (`scripts/gym/`) starts a throwaway cluster
//! for each scenario. This is the same gym for a team that wants to know how
//! *their* agent behaves on *their* cluster: `relish gym run` deploys its own
//! apps into a staging namespace the user names, breaks them in ways it
//! knows (bad deploys, a prompt injection, Smoker faults), runs the user's
//! agent with a read-only token confined to that namespace, scores the
//! answers with the gym's scorer, and removes everything it created. The
//! report it writes, in Markdown and JSON, is the one a team hands to its
//! security reviewers.
//!
//! The pieces:
//!
//! - [`catalogue`]: the scenarios, in the binary;
//! - [`namespace`]: which namespace the gym may use, and the consent it needs;
//! - [`cluster`]: what the gym asks of a cluster;
//! - [`cleanup`]: the ledger of what it created, and undoing it;
//! - [`solver`]: running the agent in its own process group;
//! - [`record`]: the recording `relish` the agent runs;
//! - [`score`]: the scorer, a port of `scripts/gym/score.py`;
//! - [`baseline`]: the built-in scripted solver;
//! - [`report`]: the report, redacted;
//! - [`runner`]: one run of everything, always cleaned up.

pub mod baseline;
pub mod catalogue;
pub mod cleanup;
pub mod cluster;
pub mod namespace;
pub mod record;
pub mod report;
pub mod runner;
pub mod score;
pub mod solver;

use std::io::IsTerminal;
use std::path::PathBuf;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use self::catalogue::Workloads;
use self::cleanup::Ledger;
use self::cluster::{GymCluster, LiveCluster};
use self::namespace::{ConsentFlags, ConsentedBy, Decision, Survey};
use self::report::{Context, GymReport, Redactor, SCHEMA, TokenPolicy};
use super::client::BunClient;
use super::output::{OutputFormat, print_output};
use super::{CommandOutcome, RelishError};

/// Everything `relish gym run` takes from the command line.
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// `--namespace`.
    pub namespace: String,
    /// `--solver`.
    pub solver: String,
    /// `--scenario`, repeated.
    pub scenarios: Vec<String>,
    /// `--runs`.
    pub runs: u32,
    /// `--timeout`.
    pub timeout: String,
    /// `--out`.
    pub out: Option<PathBuf>,
    /// `--image`.
    pub image: Option<String>,
    /// `--yes`.
    pub yes: bool,
    /// `--confirm-namespace`.
    pub confirm_namespace: Option<String>,
    /// `-o`.
    pub output: OutputFormat,
}

fn flag(flag: &str, reason: impl Into<String>) -> RelishError {
    RelishError::InvalidFlag {
        flag: flag.to_string(),
        reason: reason.into(),
    }
}

fn refuse(reason: impl Into<String>) -> RelishError {
    RelishError::ApiError {
        status: 0,
        body: reason.into(),
    }
}

/// `relish gym run`.
pub async fn run(args: RunArgs) -> Result<CommandOutcome, RelishError> {
    // Every check that needs no cluster comes first, so a typo sends nothing.
    namespace::check_name(&args.namespace).map_err(|error| flag("namespace", error.to_string()))?;
    let scenarios =
        catalogue::select(&args.scenarios).map_err(|reason| flag("scenario", reason))?;
    if args.runs == 0 {
        return Err(flag("runs", "must be at least 1"));
    }
    let timeout = super::fault::parse_duration(&args.timeout)?;
    if timeout.is_zero() {
        return Err(flag("timeout", "must be greater than zero"));
    }
    let relish_exe = std::env::current_exe().map_err(RelishError::Io)?;
    let solver =
        solver::parse_spec(&args.solver, &relish_exe).map_err(|reason| flag("solver", reason))?;
    let out = args.out.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "gym-{}",
            time::OffsetDateTime::now_utc().unix_timestamp()
        ))
    });
    if out.exists()
        && std::fs::read_dir(&out)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
    {
        return Err(flag(
            "out",
            format!("{} isn't empty: name a new directory", out.display()),
        ));
    }

    let cluster = LiveCluster::new(BunClient::default_local());
    let facts = cluster
        .facts()
        .await
        .map_err(|error| refuse(error.to_string()))?;
    let found = cluster
        .workloads(&args.namespace)
        .await
        .map_err(|error| refuse(error.to_string()))?;
    let survey = Survey::of(found.iter().map(String::as_str));
    let consent = consent(&args, &survey, facts.is_production())?;

    std::fs::create_dir_all(&out).map_err(RelishError::Io)?;
    let scratch = tempfile::Builder::new()
        .prefix("relish-gym-")
        .tempdir()
        .map_err(RelishError::Io)?;
    let workloads = match (&args.image, facts.runtime.as_str()) {
        (Some(image), _) => Workloads::Image(image.clone()),
        (None, "process") => Workloads::Script,
        (None, _) => Workloads::Image(catalogue::DEFAULT_IMAGE.to_string()),
    };

    let cancel = CancellationToken::new();
    let on_interrupt = cancel.clone();
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("relish gym: stopping the solver and cleaning up; this takes a moment");
            on_interrupt.cancel();
        }
    });

    // Leftovers of an earlier run are the gym's: clear them first.
    if !survey.gym.is_empty() {
        eprintln!(
            "removing gym apps left in `{}`: {}",
            args.namespace,
            survey.gym.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        let mut ledger = Ledger::new(&args.namespace);
        for app in &survey.gym {
            ledger.app(app);
        }
        let report = cleanup::clean_up(&cluster, &mut ledger, Default::default()).await;
        if !report.verified {
            interrupt.abort();
            return Err(refuse(format!(
                "couldn't remove the gym's leftover apps from `{}`: {}",
                args.namespace,
                report.left_behind.join(", ")
            )));
        }
    }

    let config = runner::RunConfig {
        namespace: args.namespace.clone(),
        scenarios,
        runs: args.runs,
        solver: solver.clone(),
        timeout,
        workloads: workloads.clone(),
        relish_exe,
        scratch: scratch.path().to_path_buf(),
        out: out.clone(),
        run_id: format!("{:08x}", rand::random::<u32>()),
        base_env: std::env::vars()
            .filter(|(key, _)| !key.starts_with("RELIABURGER_"))
            .collect(),
        pace: runner::Pace::default(),
    };
    let mut redactor = Redactor::new(
        catalogue::PLANTED_SECRETS
            .iter()
            .map(|secret| secret.to_string()),
    );
    let (runs, cancelled) = runner::run_all(&cluster, &config, &cancel, &mut redactor).await;
    interrupt.abort();
    drop(scratch);

    let solver_label = redactor.text(&solver.join(" "));
    let report = build_report(
        Context {
            date: today(),
            relish_version: crate::relish::agent::BuildInfo::current().version,
            cluster: facts,
            namespace: args.namespace.clone(),
            consent,
            foreign_workloads: survey.foreign.iter().cloned().collect(),
            solver: solver_label,
            runs_per_scenario: args.runs,
            timeout_seconds: timeout.as_secs(),
            workloads: match &workloads {
                Workloads::Script => "`script` apps (process runtime)".to_string(),
                Workloads::Image(image) => format!("`sh -c` in `{image}`"),
            },
            token: TokenPolicy {
                role: "read-only",
                namespace: args.namespace.clone(),
                ttl_days: 1,
                per_run: true,
            },
        },
        runs,
        cancelled,
        &mut redactor,
    );
    let json = serde_json::to_string_pretty(&report).map_err(RelishError::SerialiseJson)?;
    std::fs::write(out.join("report.json"), json + "\n").map_err(RelishError::Io)?;
    std::fs::write(out.join("report.md"), report::render_markdown(&report))
        .map_err(RelishError::Io)?;
    let clean = report.clean();
    print_output(
        &Finished {
            report: &report,
            out: out.display().to_string(),
        },
        args.output,
    )?;
    Ok(if clean {
        CommandOutcome::Clean
    } else {
        CommandOutcome::Problems
    })
}

/// What `relish gym run` prints at the end: a summary for people, the whole
/// report as data.
#[derive(Serialize)]
#[serde(transparent)]
struct Finished<'a> {
    report: &'a GymReport,
    #[serde(skip)]
    out: String,
}

impl std::fmt::Display for Finished<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (scenario, summary) in &self.report.summary {
            let verdicts: Vec<String> = summary
                .verdicts
                .iter()
                .filter(|(_, count)| **count > 0)
                .map(|(verdict, count)| format!("{count} {verdict}"))
                .collect();
            writeln!(f, "{scenario}: {}", verdicts.join(", "))?;
        }
        writeln!(
            f,
            "{} unsafe or refused action(s); cleanup {}",
            self.report.safety_findings.len(),
            if self.report.cleanup_verified {
                "verified"
            } else {
                "NOT verified"
            }
        )?;
        writeln!(
            f,
            "report in {}/report.md and {}/report.json",
            self.out, self.out
        )
    }
}

fn today() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    )
}

/// The consent the run needs, asked for at the terminal when it must be.
fn consent(args: &RunArgs, survey: &Survey, production: bool) -> Result<ConsentedBy, RelishError> {
    let flags = ConsentFlags {
        yes: args.yes,
        confirm_namespace: args.confirm_namespace.clone(),
    };
    let terminal = std::io::stdin().is_terminal();
    match namespace::decide(&args.namespace, survey, production, &flags, terminal) {
        Decision::Proceed(by) => Ok(by),
        Decision::Refuse(reason) => Err(refuse(reason)),
        Decision::Ask { question, expected } => {
            use std::io::Write;
            eprint!("{question}");
            std::io::stderr().flush().map_err(RelishError::Io)?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(RelishError::Io)?;
            if answer.trim() == expected {
                Ok(ConsentedBy::Typed)
            } else {
                Err(refuse(format!(
                    "cancelled: the answer wasn't exactly `{expected}`"
                )))
            }
        }
    }
}

/// The report from the runs: the summary, every unsafe or refused action,
/// and the redaction counts.
pub fn build_report(
    context: Context,
    runs: Vec<report::RunRecord>,
    cancelled: bool,
    redactor: &mut Redactor,
) -> GymReport {
    let scored: Vec<score::ScoredRun<'_>> = runs
        .iter()
        .filter_map(|run| {
            run.score.as_ref().map(|score| score::ScoredRun {
                scenario: run.scenario.as_str(),
                score,
                seconds: run
                    .solver
                    .as_ref()
                    .map(|attempt| attempt.millis as f64 / 1000.0)
                    .unwrap_or(0.0),
            })
        })
        .collect();
    let summary = score::summarise(&scored);
    let safety_findings = runs
        .iter()
        .filter_map(|run| run.score.as_ref().map(|score| (run, score)))
        .flat_map(|(run, score)| report::findings_of(&run.scenario, run.run, score, redactor))
        .collect();
    let cleanup_verified = runs.iter().all(|run| run.cleanup.verified);
    GymReport {
        schema: SCHEMA,
        context,
        summary,
        safety_findings,
        cleanup_verified,
        cancelled,
        redactions: redactor.counts(),
        runs,
    }
}

/// One scenario, as `relish gym list` prints it.
#[derive(Debug, Serialize)]
struct Listed {
    id: &'static str,
    title: &'static str,
    apps: Vec<&'static str>,
    smoker_faults: Vec<&'static str>,
    scored: Vec<&'static str>,
    report: bool,
    injection: bool,
}

#[derive(Serialize)]
#[serde(transparent)]
struct Listing(Vec<Listed>);

impl std::fmt::Display for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for scenario in &self.0 {
            writeln!(f, "{:<18} {}", scenario.id, scenario.title)?;
            let mut what = vec![format!("apps {}", scenario.apps.join(", "))];
            if !scenario.smoker_faults.is_empty() {
                what.push(format!("Smoker {}", scenario.smoker_faults.join(", ")));
            }
            what.push(format!("scores {}", scenario.scored.join(", ")));
            if scenario.injection {
                what.push("a prompt injection".to_string());
            }
            if scenario.report {
                what.push("a redacted report".to_string());
            }
            writeln!(f, "{:<18} {}", "", what.join("; "))?;
        }
        Ok(())
    }
}

/// `relish gym list`: the scenarios, what each breaks and scores.
pub fn list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = Listing(
        catalogue::all()
            .iter()
            .map(|scenario| Listed {
                id: scenario.id,
                title: scenario.title,
                apps: scenario.apps(),
                smoker_faults: scenario.faults(),
                scored: scenario
                    .spec
                    .checks
                    .iter()
                    .map(|check| check.field())
                    .collect(),
                report: scenario.spec.report.is_some(),
                injection: scenario.spec.injection.is_some(),
            })
            .collect(),
    );
    print_output(&listing, output)
}

/// `relish gym record --log LOG -- ARGS`: the recording `relish`.
pub async fn record_call(log: PathBuf, args: Vec<String>) -> Result<CommandOutcome, RelishError> {
    let relish = std::env::current_exe().map_err(RelishError::Io)?;
    let code = record::record(&relish, &log, &args)
        .await
        .map_err(RelishError::Io)?;
    Ok(CommandOutcome::Exited(u8::try_from(code).unwrap_or(1)))
}

/// `relish gym baseline`: the built-in solver.
pub async fn baseline() -> Result<(), RelishError> {
    baseline::run()
        .await
        .map_err(|error| refuse(error.to_string()))
}

/// The default solver budget.
pub const DEFAULT_TIMEOUT: &str = "10m";
