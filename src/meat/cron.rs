//! Cron schedule parsing and matching for scheduled jobs.
//!
//! A `schedule = "0 3 * * *"` on a job means "run at 03:00 UTC every day".
//! We parse the classic five-field crontab syntax (minute, hour, day-of-month,
//! month, day-of-week) and answer one question per event-loop minute: does this
//! schedule fire *now*? There is no external cron daemon and no new dependency;
//! the calendar arithmetic comes from the `time` crate we already pull in.
//!
//! The dialect follows Kubernetes CronJob (the `robfig/cron` parser): `N/step`
//! repeats from N to the field's maximum, a term starting with `*` leaves its
//! day field unrestricted for the day-of-month/day-of-week OR rule, month and
//! weekday names (`JAN`, `MON`) work, and so do the `@hourly`, `@daily`,
//! `@weekly`, `@monthly` and `@yearly` macros. Every time is UTC.

use std::collections::BTreeSet;

use time::{OffsetDateTime, Weekday};

/// A parse failure for a cron expression. Carries enough context to point the
/// operator at the offending field.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CronError {
    /// The expression did not have exactly five whitespace-separated fields.
    #[error("expected 5 fields (minute hour day-of-month month day-of-week), got {got}")]
    FieldCount { got: usize },

    /// A field contained a value outside its allowed range.
    #[error("field {field:?} value {value} out of range {min}..={max}")]
    OutOfRange {
        field: &'static str,
        value: u32,
        min: u8,
        max: u8,
    },

    /// A field was syntactically malformed (bad range, bad step, unknown name).
    #[error("field {field:?} is malformed: {token:?}")]
    Malformed { field: &'static str, token: String },

    /// An `@` shorthand we don't run, such as `@reboot` or `@every 5m`.
    #[error(
        "unsupported cron macro {name:?}; use @hourly, @daily, @midnight, @weekly, @monthly, @yearly, @annually or five fields"
    )]
    UnsupportedMacro { name: String },
}

/// One parsed cron field: the set of matching values, plus whether the field
/// was written starting with `*`. Remembering the star is what lets us
/// implement the day-of-month / day-of-week OR rule below: `*/2` still
/// restricts the values, but it counts as "unrestricted" for that rule, just
/// as it does in Kubernetes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    star: bool,
    values: BTreeSet<u8>,
}

impl CronField {
    fn matches(&self, value: u8) -> bool {
        self.values.contains(&value)
    }
}

/// A parsed cron schedule, matched against UTC wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
}

/// Bounds for each field, in cron order. Day-of-week accepts 0..=7 on input
/// (both 0 and 7 mean Sunday); we normalise 7 to 0 while parsing.
const BOUNDS: [(&str, u8, u8); 5] = [
    ("minute", 0, 59),
    ("hour", 0, 23),
    ("day-of-month", 1, 31),
    ("month", 1, 12),
    ("day-of-week", 0, 7),
];

/// Month names, numbered from 1.
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// Weekday names, numbered from 0 (Sunday).
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// The five-field expansion of each supported `@` macro.
fn expand_macro(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => Some("0 0 1 1 *"),
        "@monthly" => Some("0 0 1 * *"),
        "@weekly" => Some("0 0 * * 0"),
        "@daily" | "@midnight" => Some("0 0 * * *"),
        "@hourly" => Some("0 * * * *"),
        _ => None,
    }
}

impl CronSchedule {
    /// Parse a five-field cron expression (`"minute hour dom month dow"`) or
    /// one of the `@` macros.
    ///
    /// Supports `*`, single values, comma lists (`1,15,30`), ranges (`1-5`),
    /// steps (`*/15`, `0-30/10`, `5/15`), month names (`JAN`-`DEC`) and
    /// weekday names (`SUN`-`SAT`), case-insensitively.
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let trimmed = expression.trim();
        let expression = if trimmed.starts_with('@') {
            expand_macro(trimmed).ok_or_else(|| CronError::UnsupportedMacro {
                name: trimmed.to_string(),
            })?
        } else {
            expression
        };
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::FieldCount { got: fields.len() });
        }

        let parsed: Vec<CronField> = fields
            .iter()
            .zip(BOUNDS.iter())
            .map(|(token, &(name, min, max))| parse_field(token, name, min, max))
            .collect::<Result<_, _>>()?;

        Ok(CronSchedule {
            minute: parsed[0].clone(),
            hour: parsed[1].clone(),
            day_of_month: parsed[2].clone(),
            month: parsed[3].clone(),
            day_of_week: parsed[4].clone(),
        })
    }

    /// Does this schedule fire at the given instant, to minute resolution?
    ///
    /// When both day-of-month and day-of-week are restricted (neither starts
    /// with `*`), a match on *either* fires: the historical Vixie-cron
    /// behaviour that makes `0 0 1 * MON` mean "the 1st or any Monday", not
    /// "Mondays that fall on the 1st".
    pub fn matches(&self, at: OffsetDateTime) -> bool {
        let minute_ok = self.minute.matches(at.minute());
        let hour_ok = self.hour.matches(at.hour());
        let month_ok = self.month.matches(u8::from(at.month()));

        let dom = at.day();
        let dow = weekday_number(at.weekday());
        let day_ok = if self.day_of_month.star || self.day_of_week.star {
            self.day_of_month.matches(dom) && self.day_of_week.matches(dow)
        } else {
            self.day_of_month.matches(dom) || self.day_of_week.matches(dow)
        };

        minute_ok && hour_ok && day_ok && month_ok
    }
}

/// Cron numbers weekdays 0..=6 with Sunday = 0. `time` numbers them from Monday,
/// so translate.
fn weekday_number(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Sunday => 0,
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
    }
}

fn parse_field(token: &str, field: &'static str, min: u8, max: u8) -> Result<CronField, CronError> {
    let malformed = || CronError::Malformed {
        field,
        token: token.to_string(),
    };

    let mut values = BTreeSet::new();
    // Like robfig/cron, a field counts as a star when it starts with `*`.
    let star = token.starts_with('*');
    // A field is a comma list of terms; each term is a value, a range, or either
    // of those with a trailing `/step`.
    for term in token.split(',') {
        let (base, step) = match term.split_once('/') {
            Some((base, step)) => {
                let step: u8 = step.parse().map_err(|_| malformed())?;
                if step == 0 {
                    return Err(malformed());
                }
                (base, Some(step))
            }
            None => (term, None),
        };

        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((lo, hi)) = base.split_once('-') {
            (
                parse_value(lo, field, min, max)?,
                parse_value(hi, field, min, max)?,
            )
        } else {
            let v = parse_value(base, field, min, max)?;
            // `N/step` repeats from N up to the field's maximum.
            (v, if step.is_some() { max } else { v })
        };

        if start > end {
            return Err(malformed());
        }
        for value in (start..=end).step_by(usize::from(step.unwrap_or(1))) {
            values.insert(normalise(field, value));
        }
    }

    Ok(CronField { star, values })
}

fn parse_value(token: &str, field: &'static str, min: u8, max: u8) -> Result<u8, CronError> {
    let names: &[&str] = match field {
        "month" => &MONTHS,
        "day-of-week" => &WEEKDAYS,
        _ => &[],
    };
    if let Some(position) = names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(token))
    {
        // Months count from 1, weekdays from 0 (Sunday), which is `min`.
        // The tables are at most 12 long, so the cast can't truncate.
        return Ok(min + position as u8);
    }
    let value: u32 = token.parse().map_err(|_| CronError::Malformed {
        field,
        token: token.to_string(),
    })?;
    if value < u32::from(min) || value > u32::from(max) {
        return Err(CronError::OutOfRange {
            field,
            value,
            min,
            max,
        });
    }
    Ok(value as u8)
}

/// Fold day-of-week 7 (Sunday) onto 0 so matching only ever sees 0..=6.
fn normalise(field: &'static str, value: u8) -> u8 {
    if field == "day-of-week" && value == 7 {
        0
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(expr: &str) -> CronSchedule {
        CronSchedule::parse(expr).unwrap()
    }

    /// Build a UTC instant without the `time` crate's `macros` feature (which
    /// would pull an extra proc-macro dependency just for tests).
    fn utc(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> OffsetDateTime {
        let month = time::Month::try_from(month).unwrap();
        time::Date::from_calendar_date(year, month, day)
            .unwrap()
            .with_hms(hour, minute, 0)
            .unwrap()
            .assume_utc()
    }

    #[test]
    fn every_minute_matches_any_time() {
        let s = schedule("* * * * *");
        assert!(s.matches(utc(2026, 8, 13, 9, 37)));
    }

    #[test]
    fn daily_at_three_matches_only_that_minute() {
        let s = schedule("0 3 * * *");
        assert!(s.matches(utc(2026, 8, 13, 3, 0)));
        assert!(!s.matches(utc(2026, 8, 13, 3, 1)));
        assert!(!s.matches(utc(2026, 8, 13, 4, 0)));
    }

    #[test]
    fn step_fires_every_quarter_hour() {
        let s = schedule("*/15 * * * *");
        assert!(s.matches(utc(2026, 8, 13, 9, 0)));
        assert!(s.matches(utc(2026, 8, 13, 9, 15)));
        assert!(s.matches(utc(2026, 8, 13, 9, 30)));
        assert!(!s.matches(utc(2026, 8, 13, 9, 16)));
    }

    #[test]
    fn large_steps_do_not_wrap_into_other_minutes() {
        for expression in ["59/255 * * * *", "59-59/255 * * * *"] {
            let s = schedule(expression);
            for minute in 0..60 {
                assert_eq!(s.matches(utc(2026, 8, 13, 9, minute)), minute == 59);
            }
        }
        let s = schedule("*/255 * * * *");
        for minute in 0..60 {
            assert_eq!(s.matches(utc(2026, 8, 13, 9, minute)), minute == 0);
        }
    }

    #[test]
    fn range_and_list_combine() {
        let s = schedule("0 9-11,17 * * *");
        for hour in [9, 10, 11, 17] {
            let at = OffsetDateTime::from_unix_timestamp(0).unwrap()
                + time::Duration::hours(hour)
                + time::Duration::minutes(0);
            assert!(s.hour.matches(at.hour()), "hour {hour} should match");
        }
        assert!(!s.hour.matches(12));
    }

    #[test]
    fn day_of_week_sunday_accepts_zero_and_seven() {
        // 2026-08-16 is a Sunday.
        let zero = schedule("0 0 * * 0");
        let seven = schedule("0 0 * * 7");
        let sunday = utc(2026, 8, 16, 0, 0);
        assert!(zero.matches(sunday));
        assert!(seven.matches(sunday));
    }

    #[test]
    fn dom_and_dow_both_restricted_is_an_or() {
        // "1st of the month OR any Monday". 2026-08-01 is a Saturday (matches
        // via day-of-month); 2026-08-03 is a Monday (matches via day-of-week).
        let s = schedule("0 0 1 * 1");
        assert!(s.matches(utc(2026, 8, 1, 0, 0)));
        assert!(s.matches(utc(2026, 8, 3, 0, 0)));
        assert!(!s.matches(utc(2026, 8, 4, 0, 0)));
    }

    #[test]
    fn month_restriction_matches() {
        let s = schedule("0 0 1 1 *");
        assert!(s.matches(utc(2026, 1, 1, 0, 0)));
        assert!(!s.matches(utc(2026, 2, 1, 0, 0)));
    }

    #[test]
    fn wrong_field_count_is_rejected() {
        assert_eq!(
            CronSchedule::parse("* * * *"),
            Err(CronError::FieldCount { got: 4 })
        );
        assert_eq!(
            CronSchedule::parse("* * * * * *"),
            Err(CronError::FieldCount { got: 6 })
        );
    }

    #[test]
    fn out_of_range_is_rejected() {
        assert!(matches!(
            CronSchedule::parse("60 * * * *"),
            Err(CronError::OutOfRange {
                field: "minute",
                value: 60,
                ..
            })
        ));
        assert!(matches!(
            CronSchedule::parse("* 24 * * *"),
            Err(CronError::OutOfRange { field: "hour", .. })
        ));
        assert!(matches!(
            CronSchedule::parse("* * * 13 *"),
            Err(CronError::OutOfRange { field: "month", .. })
        ));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert!(matches!(
            CronSchedule::parse("* * * * abc"),
            Err(CronError::Malformed { .. })
        ));
        assert!(matches!(
            CronSchedule::parse("*/0 * * * *"),
            Err(CronError::Malformed { .. })
        ));
        assert!(matches!(
            CronSchedule::parse("5-1 * * * *"),
            Err(CronError::Malformed { .. })
        ));
    }

    #[test]
    fn start_slash_step_repeats_to_the_field_maximum() {
        // Kubernetes (robfig/cron) reads `5/15` as 5, 20, 35 and 50.
        let s = schedule("5/15 * * * *");
        for minute in 0..60 {
            assert_eq!(
                s.matches(utc(2026, 8, 13, 9, minute)),
                [5, 20, 35, 50].contains(&minute),
                "minute {minute}"
            );
        }
        let hours = schedule("0 1/6 * * *");
        for hour in 0..24 {
            assert_eq!(
                hours.matches(utc(2026, 8, 13, hour, 0)),
                [1, 7, 13, 19].contains(&hour),
                "hour {hour}"
            );
        }
    }

    #[test]
    fn star_slash_step_day_fields_are_unrestricted_for_the_or_rule() {
        // `*/2` in day-of-month still starts with `*`, so the OR rule doesn't
        // apply: the job fires on odd days that are also Mondays.
        // 2026-08-03 is a Monday (odd day), 2026-08-10 a Monday (even day),
        // 2026-08-05 a Wednesday (odd day).
        let s = schedule("0 0 */2 * 1");
        assert!(s.matches(utc(2026, 8, 3, 0, 0)));
        assert!(!s.matches(utc(2026, 8, 10, 0, 0)));
        assert!(!s.matches(utc(2026, 8, 5, 0, 0)));
        // Day-of-week `*/2` (Sunday, Tuesday, Thursday, Saturday) with a
        // restricted day-of-month: both must match.
        let s = schedule("0 0 1 * */2");
        assert!(s.matches(utc(2026, 8, 1, 0, 0))); // Saturday the 1st
        assert!(!s.matches(utc(2026, 6, 1, 0, 0))); // Monday the 1st
        assert!(!s.matches(utc(2026, 8, 4, 0, 0))); // Tuesday the 4th
    }

    #[test]
    fn daily_macro_is_supported_or_refused_clearly() {
        for (named, expanded) in [
            ("@yearly", "0 0 1 1 *"),
            ("@annually", "0 0 1 1 *"),
            ("@monthly", "0 0 1 * *"),
            ("@weekly", "0 0 * * 0"),
            ("@daily", "0 0 * * *"),
            ("@midnight", "0 0 * * *"),
            ("@hourly", "0 * * * *"),
        ] {
            assert_eq!(schedule(named), schedule(expanded), "{named}");
        }
        assert_eq!(schedule("@DAILY"), schedule("0 0 * * *"));
        for unsupported in ["@reboot", "@every 5m", "@fortnightly"] {
            let error = CronSchedule::parse(unsupported).unwrap_err();
            assert!(
                matches!(error, CronError::UnsupportedMacro { .. }),
                "{unsupported}: {error}"
            );
            assert!(error.to_string().contains("@hourly"), "{error}");
        }
    }

    #[test]
    fn month_and_weekday_names_match_their_numbers() {
        assert_eq!(schedule("0 0 1 JAN MON"), schedule("0 0 1 1 1"));
        assert_eq!(schedule("0 0 * jun-aug sat,sun"), schedule("0 0 * 6-8 6,0"));
        assert_eq!(schedule("0 9 * * Mon-Fri"), schedule("0 9 * * 1-5"));
        assert!(matches!(
            CronSchedule::parse("0 0 * * FOO"),
            Err(CronError::Malformed { .. })
        ));
        // Names belong only to their own field.
        assert!(matches!(
            CronSchedule::parse("0 0 * MON *"),
            Err(CronError::Malformed { .. })
        ));
    }
}
