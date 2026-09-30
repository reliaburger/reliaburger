//! Human-readable units for the dashboard's charts.
//!
//! A chart's values arrive in base units: bytes, seconds, requests per
//! second. Nobody wants to read `0.0003` on a latency axis or `10M` next to
//! the word "bytes". Each [`ChartUnit`] owns a ladder of [`UnitStep`]s
//! (bytes → KiB → MiB …, seconds → µs → ms …), and a value is shown in the
//! largest step that doesn't exceed it.
//!
//! The ladders live here, in Rust, and travel to the browser inside each
//! chart's `data-chart-config`. `brioche.js` runs the same two small rules
//! as [`ChartUnit::format`] and [`ChartUnit::format_axis`] over whatever
//! ladder it's given, so the boundaries tested below are the ones the
//! browser draws.

use serde::Serialize;
use serde::ser::Serializer;

/// One rung of a unit ladder: values at least `factor` are divided by it
/// and shown with `suffix`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct UnitStep {
    /// What one of this unit is worth in the base unit.
    pub factor: f64,
    /// Appended to the scaled number, including any separating space.
    pub suffix: &'static str,
}

/// What a chart's values measure, which decides how its axis and legend
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartUnit {
    /// A percentage, already scaled to 0–100 (CPU).
    Percent,
    /// Bytes, in binary multiples (KiB, MiB, GiB …).
    Bytes,
    /// A duration in seconds, shown from nanoseconds up to seconds.
    Seconds,
    /// Requests per second, in decimal multiples.
    RequestsPerSecond,
    /// A plain count or anything whose unit we don't know.
    Number,
}

const fn step(factor: f64, suffix: &'static str) -> UnitStep {
    UnitStep { factor, suffix }
}

const KIB: f64 = 1024.0;

const PERCENT: [UnitStep; 1] = [step(1.0, "%")];
const BYTES: [UnitStep; 6] = [
    step(1.0, " B"),
    step(KIB, " KiB"),
    step(KIB * KIB, " MiB"),
    step(KIB * KIB * KIB, " GiB"),
    step(KIB * KIB * KIB * KIB, " TiB"),
    step(KIB * KIB * KIB * KIB * KIB, " PiB"),
];
const SECONDS: [UnitStep; 4] = [
    step(1e-9, " ns"),
    step(1e-6, " µs"),
    step(1e-3, " ms"),
    step(1.0, " s"),
];
const REQUESTS_PER_SECOND: [UnitStep; 4] = [
    step(1.0, " req/s"),
    step(1e3, "k req/s"),
    step(1e6, "M req/s"),
    step(1e9, "G req/s"),
];
const NUMBER: [UnitStep; 4] = [
    step(1.0, ""),
    step(1e3, "k"),
    step(1e6, "M"),
    step(1e9, "G"),
];

impl ChartUnit {
    /// The ladder, smallest step first. Every ladder has a step with
    /// factor 1, which is where zero is shown.
    pub fn steps(self) -> &'static [UnitStep] {
        match self {
            ChartUnit::Percent => &PERCENT,
            ChartUnit::Bytes => &BYTES,
            ChartUnit::Seconds => &SECONDS,
            ChartUnit::RequestsPerSecond => &REQUESTS_PER_SECOND,
            ChartUnit::Number => &NUMBER,
        }
    }

    /// One value in its own best step, as the legend shows it:
    /// `1023 B`, `1 KiB`, `999 µs`, `1 ms`.
    pub fn format(self, value: f64) -> String {
        format_in(pick_step(self.steps(), value.abs()), value)
    }

    /// Axis tick labels. All ticks share the step chosen for the largest
    /// one, so an axis reads `0.5 MiB, 1 MiB, 1.5 MiB` rather than mixing
    /// `512 KiB` with `1 MiB`.
    pub fn format_axis(self, splits: &[f64]) -> Vec<String> {
        let largest = splits.iter().fold(0.0_f64, |acc, v| acc.max(v.abs()));
        let chosen = pick_step(self.steps(), largest);
        splits.iter().map(|v| format_in(chosen, *v)).collect()
    }
}

/// The browser gets the ladder itself, not a name it would have to know.
impl Serialize for ChartUnit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.steps().serialize(serializer)
    }
}

/// The largest step whose factor doesn't exceed `magnitude`; zero uses the
/// base (factor 1) step, anything smaller than the first step uses that.
fn pick_step(steps: &'static [UnitStep], magnitude: f64) -> UnitStep {
    let mut chosen = steps[0];
    for candidate in steps {
        let fits = if magnitude == 0.0 {
            candidate.factor == 1.0
        } else {
            candidate.factor <= magnitude
        };
        if fits {
            chosen = *candidate;
        }
    }
    chosen
}

/// Three significant figures at most, trailing zeros dropped. Mirrors
/// `String(Number(scaled.toFixed(decimals)))` in brioche.js.
fn format_in(unit: UnitStep, value: f64) -> String {
    let scaled = value / unit.factor;
    let magnitude = scaled.abs();
    let decimals = if magnitude >= 100.0 {
        0
    } else if magnitude >= 10.0 {
        1
    } else {
        2
    };
    let mut number = format!("{scaled:.decimals$}");
    if number.contains('.') {
        number = number
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
    }
    if number == "-0" {
        number = "0".to_string();
    }
    format!("{number}{}", unit.suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_switch_to_kib_at_exactly_1024() {
        assert_eq!(ChartUnit::Bytes.format(1023.0), "1023 B");
        assert_eq!(ChartUnit::Bytes.format(1024.0), "1 KiB");
        assert_eq!(ChartUnit::Bytes.format(1536.0), "1.5 KiB");
    }

    #[test]
    fn bytes_climb_through_mib_and_gib() {
        assert_eq!(ChartUnit::Bytes.format(10.0 * 1024.0 * 1024.0), "10 MiB");
        assert_eq!(
            ChartUnit::Bytes.format(1.2 * 1024.0 * 1024.0 * 1024.0),
            "1.2 GiB"
        );
    }

    #[test]
    fn very_large_values_stay_in_the_top_step() {
        assert_eq!(ChartUnit::Bytes.format(2.0 * 1024f64.powi(5)), "2 PiB");
        assert_eq!(ChartUnit::Bytes.format(1e20), "88818 PiB");
        assert_eq!(ChartUnit::Seconds.format(86_400.0), "86400 s");
        assert_eq!(ChartUnit::RequestsPerSecond.format(3e12), "3000G req/s");
    }

    #[test]
    fn durations_switch_to_milliseconds_at_exactly_one() {
        assert_eq!(ChartUnit::Seconds.format(999e-6), "999 µs");
        assert_eq!(ChartUnit::Seconds.format(1e-3), "1 ms");
        assert_eq!(ChartUnit::Seconds.format(0.0003), "300 µs");
        assert_eq!(ChartUnit::Seconds.format(0.012), "12 ms");
        assert_eq!(ChartUnit::Seconds.format(1.5), "1.5 s");
        assert_eq!(ChartUnit::Seconds.format(42e-9), "42 ns");
    }

    #[test]
    fn zero_reads_in_the_base_unit() {
        assert_eq!(ChartUnit::Bytes.format(0.0), "0 B");
        assert_eq!(ChartUnit::Seconds.format(0.0), "0 s");
        assert_eq!(ChartUnit::Seconds.format(-0.0), "0 s");
        assert_eq!(ChartUnit::Percent.format(0.0), "0%");
        assert_eq!(ChartUnit::RequestsPerSecond.format(0.0), "0 req/s");
    }

    #[test]
    fn values_below_the_smallest_step_use_it() {
        assert_eq!(ChartUnit::Bytes.format(0.5), "0.5 B");
        assert_eq!(ChartUnit::Seconds.format(1e-10), "0.1 ns");
        assert_eq!(ChartUnit::RequestsPerSecond.format(0.25), "0.25 req/s");
    }

    #[test]
    fn rates_and_counts_use_decimal_multiples() {
        assert_eq!(ChartUnit::RequestsPerSecond.format(12.0), "12 req/s");
        assert_eq!(ChartUnit::RequestsPerSecond.format(1500.0), "1.5k req/s");
        assert_eq!(ChartUnit::Number.format(999.0), "999");
        assert_eq!(ChartUnit::Number.format(1000.0), "1k");
        assert_eq!(ChartUnit::Number.format(2.5e6), "2.5M");
    }

    #[test]
    fn percentages_never_scale() {
        assert_eq!(ChartUnit::Percent.format(12.345), "12.3%");
        assert_eq!(ChartUnit::Percent.format(250.0), "250%");
    }

    #[test]
    fn three_significant_figures_at_most() {
        assert_eq!(ChartUnit::Number.format(1.23456), "1.23");
        assert_eq!(ChartUnit::Number.format(12.3456), "12.3");
        assert_eq!(ChartUnit::Number.format(123.456), "123");
        assert_eq!(ChartUnit::Number.format(-12.3456), "-12.3");
    }

    #[test]
    fn an_axis_shares_one_step_chosen_by_its_largest_tick() {
        let mib = 1024.0 * 1024.0;
        assert_eq!(
            ChartUnit::Bytes.format_axis(&[0.0, 0.5 * mib, mib, 1.5 * mib]),
            ["0 MiB", "0.5 MiB", "1 MiB", "1.5 MiB"]
        );
        assert_eq!(
            ChartUnit::Seconds.format_axis(&[0.0, 0.0005, 0.001, 0.0015]),
            ["0 ms", "0.5 ms", "1 ms", "1.5 ms"]
        );
        assert_eq!(ChartUnit::Seconds.format_axis(&[0.0]), ["0 s"]);
        assert!(ChartUnit::Seconds.format_axis(&[]).is_empty());
    }

    #[test]
    fn every_ladder_has_a_base_step_and_climbs() {
        for unit in [
            ChartUnit::Percent,
            ChartUnit::Bytes,
            ChartUnit::Seconds,
            ChartUnit::RequestsPerSecond,
            ChartUnit::Number,
        ] {
            let steps = unit.steps();
            assert!(steps.iter().any(|s| s.factor == 1.0), "{unit:?}");
            assert!(
                steps.windows(2).all(|w| w[0].factor < w[1].factor),
                "{unit:?}"
            );
        }
    }

    #[test]
    fn a_unit_serialises_as_its_ladder() {
        let json = serde_json::to_string(&ChartUnit::Seconds).unwrap();
        assert_eq!(
            json,
            r#"[{"factor":1e-9,"suffix":" ns"},{"factor":1e-6,"suffix":" µs"},{"factor":0.001,"suffix":" ms"},{"factor":1.0,"suffix":" s"}]"#
        );
    }
}
