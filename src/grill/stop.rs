//! How an instance is asked to stop (#1121).
//!
//! An app can name the signal that asks it to stop, how long it then gets
//! before it's killed, and a command to run inside it first. The config
//! fields are parsed straight into these types, so a value that would be
//! refused never reaches an `AppSpec`: an unknown signal or a zero grace is
//! a parse error with the field's line, not a surprise at stop time.
//!
//! [`StopPolicy`] travels in the instance's [`OciSpec`](super::OciSpec), so
//! every runtime sends the right signal on any stop, and an adopted instance
//! keeps its policy across a Bun restart.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;

/// The longest grace or pre-stop bound an app may ask for. A stop that may
/// take longer than an hour is a stuck stop, and every caller that waits on
/// one (a deploy step, a node drain) would wait with it.
pub const MAX_STOP_DURATION: Duration = Duration::from_secs(3600);

/// How long `pre_stop` may run when the app doesn't say. The same as
/// Kubernetes' default `terminationGracePeriodSeconds`.
pub const DEFAULT_PRE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// A signal that asks a workload to stop.
///
/// Only signals a program can catch and treat as "please exit" are here.
/// `SIGKILL` isn't: it's what the grace escalates to. Numbers aren't
/// accepted either, because they differ between Linux and macOS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum StopSignal {
    /// `SIGTERM`, the default, as everywhere else.
    #[default]
    Term,
    /// `SIGINT`, what Ctrl-C sends. Many dev servers only handle this one.
    Int,
    /// `SIGQUIT`. nginx's graceful shutdown.
    Quit,
    /// `SIGHUP`.
    Hup,
    /// `SIGUSR1`.
    Usr1,
    /// `SIGUSR2`.
    Usr2,
    /// `SIGWINCH`. Apache httpd's graceful stop.
    Winch,
}

impl StopSignal {
    /// Every accepted signal, in the order error messages list them.
    pub const ALL: [StopSignal; 7] = [
        StopSignal::Term,
        StopSignal::Int,
        StopSignal::Quit,
        StopSignal::Hup,
        StopSignal::Usr1,
        StopSignal::Usr2,
        StopSignal::Winch,
    ];

    /// The canonical name, such as `"SIGTERM"`. Runc's `kill` and Apple's
    /// `container stop --signal` both take it as it is.
    pub fn name(self) -> &'static str {
        match self {
            StopSignal::Term => "SIGTERM",
            StopSignal::Int => "SIGINT",
            StopSignal::Quit => "SIGQUIT",
            StopSignal::Hup => "SIGHUP",
            StopSignal::Usr1 => "SIGUSR1",
            StopSignal::Usr2 => "SIGUSR2",
            StopSignal::Winch => "SIGWINCH",
        }
    }

    /// The signal as `kill(2)` takes it on this platform.
    pub fn to_nix(self) -> nix::sys::signal::Signal {
        use nix::sys::signal::Signal;
        match self {
            StopSignal::Term => Signal::SIGTERM,
            StopSignal::Int => Signal::SIGINT,
            StopSignal::Quit => Signal::SIGQUIT,
            StopSignal::Hup => Signal::SIGHUP,
            StopSignal::Usr1 => Signal::SIGUSR1,
            StopSignal::Usr2 => Signal::SIGUSR2,
            StopSignal::Winch => Signal::SIGWINCH,
        }
    }
}

impl fmt::Display for StopSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A stop setting that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopSettingError {
    #[error("unknown stop signal {0:?}: use one of {list}", list = signal_list())]
    UnknownSignal(String),
    #[error("{0:?} is not a duration: use seconds, minutes or hours, such as \"30s\" or \"2m\"")]
    UnparseableDuration(String),
    #[error("{0:?} is too short: a stop step needs at least 1s")]
    TooShort(String),
    #[error("{value:?} is too long: the most a stop step may take is {max}s", max = MAX_STOP_DURATION.as_secs())]
    TooLong { value: String },
}

fn signal_list() -> String {
    StopSignal::ALL
        .iter()
        .map(|signal| signal.name())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FromStr for StopSignal {
    type Err = StopSettingError;

    /// Accept `SIGINT`, `INT` or `sigint`; refuse anything else.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let upper = value.trim().to_ascii_uppercase();
        let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
        StopSignal::ALL
            .into_iter()
            .find(|signal| &signal.name()[3..] == bare)
            .ok_or_else(|| StopSettingError::UnknownSignal(value.to_string()))
    }
}

impl TryFrom<String> for StopSignal {
    type Error = StopSettingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<StopSignal> for String {
    fn from(signal: StopSignal) -> Self {
        signal.name().to_string()
    }
}

/// A stop grace or pre-stop bound: whole seconds, from 1s to an hour.
///
/// Written like the other durations in an app (`"30s"`, `"2m"`, `"1h"`)
/// and written back as seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StopDuration(Duration);

impl StopDuration {
    /// A bound of `duration`, if it's in range.
    pub fn new(duration: Duration) -> Result<Self, StopSettingError> {
        let shown = || format!("{}s", duration.as_secs());
        if duration < Duration::from_secs(1) {
            return Err(StopSettingError::TooShort(shown()));
        }
        if duration > MAX_STOP_DURATION {
            return Err(StopSettingError::TooLong { value: shown() });
        }
        Ok(Self(duration))
    }

    /// The bound itself.
    pub fn duration(self) -> Duration {
        self.0
    }
}

impl FromStr for StopDuration {
    type Err = StopSettingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let duration = crate::meat::deploy_types::parse_duration(value)
            .ok_or_else(|| StopSettingError::UnparseableDuration(value.to_string()))?;
        match Self::new(duration) {
            Err(StopSettingError::TooShort(_)) => Err(StopSettingError::TooShort(value.into())),
            Err(StopSettingError::TooLong { .. }) => Err(StopSettingError::TooLong {
                value: value.into(),
            }),
            result => result,
        }
    }
}

impl TryFrom<String> for StopDuration {
    type Error = StopSettingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<StopDuration> for String {
    fn from(duration: StopDuration) -> Self {
        format!("{}s", duration.0.as_secs())
    }
}

/// A command run inside the instance before its stop signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreStop {
    /// The argv, run as `relish exec` would run it.
    pub command: Vec<String>,
    /// How long it may run. When it's up, the stop carries on without it.
    pub timeout: Duration,
}

/// How one instance is asked to stop, as its app declared it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopPolicy {
    /// The signal the runtime sends on a graceful stop.
    #[serde(default)]
    pub signal: StopSignal,
    /// Time to exit after the signal. `None` keeps the caller's own: the
    /// deploy's `drain_timeout`, or the node's stop grace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<Duration>,
    /// Run before the signal, with its own bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_stop: Option<PreStop>,
}

impl StopPolicy {
    /// The policy an app declares, or `None` when it sets none of the stop
    /// fields and so stops the way every app always has.
    pub fn from_app(spec: &AppSpec) -> Option<Self> {
        let pre_stop = (!spec.pre_stop.is_empty()).then(|| PreStop {
            command: spec.pre_stop.clone(),
            timeout: spec
                .pre_stop_timeout
                .map_or(DEFAULT_PRE_STOP_TIMEOUT, StopDuration::duration),
        });
        if spec.stop_signal.is_none() && spec.stop_grace.is_none() && pre_stop.is_none() {
            return None;
        }
        Some(Self {
            signal: spec.stop_signal.unwrap_or_default(),
            grace: spec.stop_grace.map(StopDuration::duration),
            pre_stop,
        })
    }

    /// The signal `policy` asks for, SIGTERM without one.
    pub fn signal_of(policy: Option<&Self>) -> StopSignal {
        policy.map(|policy| policy.signal).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(toml_source: &str) -> Result<AppSpec, toml::de::Error> {
        toml::from_str(&format!("exec = \"/bin/sleep\"\n{toml_source}"))
    }

    #[test]
    fn stop_signal_accepts_names_with_or_without_the_sig_prefix() {
        assert_eq!("SIGINT".parse::<StopSignal>().unwrap(), StopSignal::Int);
        assert_eq!("INT".parse::<StopSignal>().unwrap(), StopSignal::Int);
        assert_eq!("sigquit".parse::<StopSignal>().unwrap(), StopSignal::Quit);
        assert_eq!("WINCH".parse::<StopSignal>().unwrap(), StopSignal::Winch);
    }

    #[test]
    fn every_stop_signal_parses_back_from_its_own_name() {
        for signal in StopSignal::ALL {
            assert_eq!(signal.name().parse::<StopSignal>().unwrap(), signal);
        }
    }

    #[test]
    fn stop_signal_refuses_unknown_names_numbers_and_sigkill() {
        for refused in ["SIGFOO", "", "15", "SIGKILL", "KILL", "SIGSTOP", "SIG"] {
            let error = refused.parse::<StopSignal>().unwrap_err();
            assert_eq!(error, StopSettingError::UnknownSignal(refused.into()));
        }
    }

    #[test]
    fn unknown_stop_signal_error_lists_the_accepted_ones() {
        let error = "SIGFOO".parse::<StopSignal>().unwrap_err().to_string();
        assert_eq!(
            error,
            "unknown stop signal \"SIGFOO\": use one of SIGTERM, SIGINT, SIGQUIT, SIGHUP, \
             SIGUSR1, SIGUSR2, SIGWINCH"
        );
    }

    #[test]
    fn stop_signal_maps_to_the_platform_signal() {
        use nix::sys::signal::Signal;
        assert_eq!(StopSignal::Term.to_nix(), Signal::SIGTERM);
        assert_eq!(StopSignal::Int.to_nix(), Signal::SIGINT);
        assert_eq!(StopSignal::Winch.to_nix(), Signal::SIGWINCH);
    }

    #[test]
    fn stop_duration_takes_seconds_minutes_and_hours() {
        let parse = |value: &str| value.parse::<StopDuration>().unwrap().duration();
        assert_eq!(parse("45s"), Duration::from_secs(45));
        assert_eq!(parse("2m"), Duration::from_secs(120));
        assert_eq!(parse("1h"), Duration::from_secs(3600));
        assert_eq!(parse("10"), Duration::from_secs(10));
    }

    #[test]
    fn stop_duration_refuses_zero_garbage_and_more_than_an_hour() {
        assert_eq!(
            "0s".parse::<StopDuration>().unwrap_err(),
            StopSettingError::TooShort("0s".into())
        );
        assert_eq!(
            "soon".parse::<StopDuration>().unwrap_err(),
            StopSettingError::UnparseableDuration("soon".into())
        );
        assert_eq!(
            "500ms".parse::<StopDuration>().unwrap_err(),
            StopSettingError::UnparseableDuration("500ms".into())
        );
        assert_eq!(
            "61m".parse::<StopDuration>().unwrap_err(),
            StopSettingError::TooLong {
                value: "61m".into()
            }
        );
    }

    #[test]
    fn stop_duration_is_written_back_as_seconds() {
        let duration: StopDuration = "2m".parse().unwrap();
        assert_eq!(String::from(duration), "120s");
    }

    #[test]
    fn an_app_without_stop_fields_has_no_policy() {
        assert_eq!(StopPolicy::from_app(&app("").unwrap()), None);
    }

    #[test]
    fn app_stop_fields_parse_into_a_policy() {
        let spec = app("stop_signal = \"SIGINT\"\nstop_grace = \"45s\"\n\
             pre_stop = [\"/bin/echo\", \"bye\"]\npre_stop_timeout = \"5s\"\n")
        .unwrap();
        assert_eq!(
            StopPolicy::from_app(&spec),
            Some(StopPolicy {
                signal: StopSignal::Int,
                grace: Some(Duration::from_secs(45)),
                pre_stop: Some(PreStop {
                    command: vec!["/bin/echo".into(), "bye".into()],
                    timeout: Duration::from_secs(5),
                }),
            })
        );
    }

    #[test]
    fn pre_stop_without_a_timeout_gets_the_default_bound() {
        let spec = app("pre_stop = [\"/bin/true\"]\n").unwrap();
        let policy = StopPolicy::from_app(&spec).unwrap();
        assert_eq!(policy.signal, StopSignal::Term);
        assert_eq!(policy.grace, None);
        assert_eq!(policy.pre_stop.unwrap().timeout, DEFAULT_PRE_STOP_TIMEOUT);
    }

    #[test]
    fn an_unknown_stop_signal_is_a_parse_error_naming_the_value() {
        let error = app("stop_signal = \"SIGFOO\"\n").unwrap_err().to_string();
        assert!(error.contains("unknown stop signal \"SIGFOO\""), "{error}");
        assert!(error.contains("stop_signal"), "{error}");
    }

    #[test]
    fn a_zero_or_absurd_stop_grace_is_a_parse_error() {
        let zero = app("stop_grace = \"0s\"\n").unwrap_err().to_string();
        assert!(zero.contains("needs at least 1s"), "{zero}");
        let absurd = app("stop_grace = \"48h\"\n").unwrap_err().to_string();
        assert!(
            absurd.contains("at most") || absurd.contains("too long"),
            "{absurd}"
        );
        let zero_bound = app("pre_stop = [\"/bin/true\"]\npre_stop_timeout = \"0\"\n")
            .unwrap_err()
            .to_string();
        assert!(zero_bound.contains("needs at least 1s"), "{zero_bound}");
    }

    #[test]
    fn stop_fields_survive_a_toml_round_trip() {
        let spec = app("stop_signal = \"int\"\nstop_grace = \"2m\"\n").unwrap();
        let written = toml::to_string(&spec).unwrap();
        assert!(written.contains("stop_signal = \"SIGINT\""), "{written}");
        assert!(written.contains("stop_grace = \"120s\""), "{written}");
        let back: AppSpec = toml::from_str(&written).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn policy_survives_a_json_round_trip() {
        let policy = StopPolicy {
            signal: StopSignal::Quit,
            grace: Some(Duration::from_secs(20)),
            pre_stop: Some(PreStop {
                command: vec!["/bin/true".into()],
                timeout: Duration::from_secs(3),
            }),
        };
        let json = serde_json::to_string(&policy).unwrap();
        assert!(json.contains("\"SIGQUIT\""), "{json}");
        assert_eq!(serde_json::from_str::<StopPolicy>(&json).unwrap(), policy);
    }
}
