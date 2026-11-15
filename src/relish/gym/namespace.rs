//! Which namespace the gym may work in, and what the user must say first.
//!
//! The rules, in the order they're checked:
//!
//! 1. The user names the namespace; there's no default.
//! 2. Never `default`, never `rbtest-*` (a `relish test` lease owns those),
//!    never anything starting `reliaburger`, and only a DNS label.
//! 3. An app whose name is one of the gym's own is the gym's (a leftover the
//!    gym removes first). Anything else in the namespace is someone else's.
//! 4. An empty or gym-only namespace needs `--yes`, or `yes` typed at a
//!    terminal. A namespace with anyone else's workloads, or any namespace
//!    on a cluster tagged `environment = "production"`, needs the namespace
//!    name repeated: `--confirm-namespace NS`, or typed at a terminal.
//!
//! Even confirmed, the gym only creates, faults and deletes its own apps.

use std::collections::BTreeSet;

use super::catalogue::GYM_APP_NAMES;

/// The longest DNS label.
const MAX_LABEL: usize = 63;

/// Why a namespace name is refused before any request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameRefusal {
    /// `default`: everyone's apps land there.
    #[error("the gym never runs in `default`; name a staging namespace, such as `agent-staging`")]
    Default,
    /// A `relish test` lease namespace.
    #[error("`{0}` is a `relish test` namespace; name a staging namespace of your own")]
    TestLease(String),
    /// Reliaburger's own.
    #[error("`{0}` is reserved for Reliaburger; name a staging namespace of your own")]
    Reserved(String),
    /// Not a DNS label.
    #[error(
        "`{0}` isn't a namespace name: use 1 to 63 lowercase letters, digits and hyphens, starting and ending with a letter or digit"
    )]
    NotALabel(String),
}

/// Check a namespace name before anything is sent.
pub fn check_name(namespace: &str) -> Result<(), NameRefusal> {
    let is_label = !namespace.is_empty()
        && namespace.len() <= MAX_LABEL
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !namespace.starts_with('-')
        && !namespace.ends_with('-');
    if !is_label {
        return Err(NameRefusal::NotALabel(namespace.to_string()));
    }
    if namespace == "default" {
        return Err(NameRefusal::Default);
    }
    if crate::testkit::TestContext::is_test_namespace(namespace) {
        return Err(NameRefusal::TestLease(namespace.to_string()));
    }
    if namespace.starts_with("reliaburger") {
        return Err(NameRefusal::Reserved(namespace.to_string()));
    }
    Ok(())
}

/// What's in the namespace now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survey {
    /// The gym's own apps, left from an earlier run.
    pub gym: BTreeSet<String>,
    /// Everyone else's apps and jobs.
    pub foreign: BTreeSet<String>,
}

impl Survey {
    /// Sort every workload name found in the namespace.
    pub fn of<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut survey = Survey::default();
        for name in names {
            if GYM_APP_NAMES.contains(&name) {
                survey.gym.insert(name.to_string());
            } else {
                survey.foreign.insert(name.to_string());
            }
        }
        survey
    }
}

/// What the user said on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsentFlags {
    /// `--yes`.
    pub yes: bool,
    /// `--confirm-namespace NS`.
    pub confirm_namespace: Option<String>,
}

/// How the user consented, for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentedBy {
    /// `--yes`.
    YesFlag,
    /// `--confirm-namespace`.
    ConfirmNamespaceFlag,
    /// Typed at a terminal.
    Typed,
}

/// What the gym must hear before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The flags already consent.
    Proceed(ConsentedBy),
    /// Ask at the terminal: `yes`, or the namespace name when `name` is set.
    Ask {
        /// The question.
        question: String,
        /// What the answer must be, exactly.
        expected: String,
    },
    /// Refuse, with what to do instead.
    Refuse(String),
}

/// Decide what consent the run needs, from the survey, the cluster's
/// environment tag, the flags and whether stdin is a terminal.
pub fn decide(
    namespace: &str,
    survey: &Survey,
    production: bool,
    flags: &ConsentFlags,
    terminal: bool,
) -> Decision {
    if let Some(confirmed) = &flags.confirm_namespace
        && confirmed != namespace
    {
        return Decision::Refuse(format!(
            "--confirm-namespace says `{confirmed}`, but the gym would run in `{namespace}`"
        ));
    }
    let needs_name = !survey.foreign.is_empty() || production;
    if needs_name {
        if flags.confirm_namespace.is_some() {
            return Decision::Proceed(ConsentedBy::ConfirmNamespaceFlag);
        }
        let mut why = Vec::new();
        if !survey.foreign.is_empty() {
            why.push(format!(
                "`{namespace}` holds workloads that aren't the gym's: {}",
                survey
                    .foreign
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if production {
            why.push("this cluster is tagged `environment = \"production\"`".to_string());
        }
        let why = why.join(", and ");
        if !terminal {
            return Decision::Refuse(format!(
                "{why}; the gym deploys, breaks and deletes only its own `gym-*` apps, but the solver will see the rest. Pass --confirm-namespace {namespace} if that's what you want"
            ));
        }
        return Decision::Ask {
            question: format!(
                "{why}. The gym deploys, breaks and deletes only its own `gym-*` apps there, and your agent will see the rest. Type the namespace name to continue: "
            ),
            expected: namespace.to_string(),
        };
    }
    if flags.yes || flags.confirm_namespace.is_some() {
        return Decision::Proceed(if flags.yes {
            ConsentedBy::YesFlag
        } else {
            ConsentedBy::ConfirmNamespaceFlag
        });
    }
    if !terminal {
        return Decision::Refuse(format!(
            "the gym deploys apps and injects Smoker faults into `{namespace}`; pass --yes to agree without a terminal"
        ));
    }
    Decision::Ask {
        question: format!(
            "The gym will deploy its own apps into `{namespace}` and inject Smoker faults into them, then remove them. Type 'yes' to continue: "
        ),
        expected: "yes".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_reserved_namespaces_are_refused_by_name() {
        assert_eq!(check_name("default"), Err(NameRefusal::Default));
        assert!(matches!(
            check_name("rbtest-abc"),
            Err(NameRefusal::TestLease(_))
        ));
        assert!(matches!(
            check_name("reliaburger-system"),
            Err(NameRefusal::Reserved(_))
        ));
        for bad in ["", "Staging", "agent_staging", "-x", "x-", &"a".repeat(64)] {
            assert!(
                matches!(check_name(bad), Err(NameRefusal::NotALabel(_))),
                "{bad}"
            );
        }
        assert_eq!(check_name("agent-staging"), Ok(()));
        assert_eq!(check_name("rbtestx"), Ok(()));
    }

    #[test]
    fn the_gyms_own_apps_are_told_apart_from_everyone_elses() {
        let survey = Survey::of(["gym-web", "checkout", "gym-api", "gym-webx"]);
        assert_eq!(
            survey.gym,
            BTreeSet::from(["gym-api".into(), "gym-web".into()])
        );
        assert_eq!(
            survey.foreign,
            BTreeSet::from(["checkout".into(), "gym-webx".into()])
        );
    }

    fn yes() -> ConsentFlags {
        ConsentFlags {
            yes: true,
            confirm_namespace: None,
        }
    }

    fn confirm(namespace: &str) -> ConsentFlags {
        ConsentFlags {
            yes: false,
            confirm_namespace: Some(namespace.to_string()),
        }
    }

    #[test]
    fn an_empty_or_gym_only_namespace_needs_yes() {
        let empty = Survey::default();
        let leftovers = Survey::of(["gym-web"]);
        for survey in [&empty, &leftovers] {
            assert_eq!(
                decide("staging", survey, false, &yes(), false),
                Decision::Proceed(ConsentedBy::YesFlag)
            );
            assert!(matches!(
                decide("staging", survey, false, &ConsentFlags::default(), false),
                Decision::Refuse(_)
            ));
            assert_eq!(
                decide("staging", survey, false, &ConsentFlags::default(), true),
                Decision::Ask {
                    question: "The gym will deploy its own apps into `staging` and inject Smoker faults into them, then remove them. Type 'yes' to continue: ".into(),
                    expected: "yes".into(),
                }
            );
        }
    }

    #[test]
    fn someone_elses_workloads_need_the_name_and_yes_is_not_enough() {
        let survey = Survey::of(["checkout"]);
        let Decision::Refuse(reason) = decide("staging", &survey, false, &yes(), false) else {
            panic!("--yes alone must not do");
        };
        assert!(reason.contains("checkout"), "{reason}");
        assert!(reason.contains("--confirm-namespace staging"), "{reason}");
        assert_eq!(
            decide("staging", &survey, false, &confirm("staging"), false),
            Decision::Proceed(ConsentedBy::ConfirmNamespaceFlag)
        );
        let Decision::Ask { expected, question } = decide("staging", &survey, false, &yes(), true)
        else {
            panic!("a terminal is asked for the name");
        };
        assert_eq!(expected, "staging");
        assert!(question.contains("checkout"), "{question}");
    }

    #[test]
    fn a_production_cluster_needs_the_name_too() {
        let empty = Survey::default();
        assert!(matches!(
            decide("staging", &empty, true, &yes(), false),
            Decision::Refuse(reason) if reason.contains("production")
        ));
        assert_eq!(
            decide("staging", &empty, true, &confirm("staging"), false),
            Decision::Proceed(ConsentedBy::ConfirmNamespaceFlag)
        );
    }

    #[test]
    fn confirming_another_namespace_is_refused() {
        let Decision::Refuse(reason) =
            decide("staging", &Survey::default(), false, &confirm("prod"), true)
        else {
            panic!("a mismatch must refuse");
        };
        assert!(reason.contains("`prod`"), "{reason}");
    }
}
