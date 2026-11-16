//! The table, and what it says at the bottom.
//!
//! A pure function from two runs to a string, which is what makes it testable
//! without measuring anything — and it is worth testing, because the report is
//! the whole product. Everything else in this crate exists to produce numbers;
//! this is the part that has to make them mean something at a glance.
//!
//! The rule it is built on: say the least that is still true. A row that did
//! not move prints its number and nothing else. Only a row that moved gets a
//! percentage, and only a row that moved beyond its own noise gets attention
//! drawn to it.

use std::fmt::Write as _;

use crate::{
    history::Run,
    stats::{Delta, Thresholds, Unit, Verdict},
};

/// One row of the table.
struct Row {
    id: String,
    median: String,
    tail: String,
    delta: Option<Delta>,
    noisy_benchmark: bool,
}

/// Renders a run, optionally against a baseline.
pub fn render(run: &Run, baseline: Option<&Run>, limits: Thresholds) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "machine  {}", run.machine.describe());
    let _ = writeln!(out, "commit   {} on {}", run.label(), run.branch);
    let _ = writeln!(out, "corpus   {} messages", Unit::Count.format(run.corpus as f64));

    match baseline {
        Some(base) => {
            let _ = writeln!(
                out,
                "baseline {} \u{2014} {}",
                base.label(),
                ago(run.recorded_at - base.recorded_at)
            );
        }
        None => {
            let _ = writeln!(out, "baseline none \u{2014} this is the first run to compare against");
        }
    }

    // Conditions go above the numbers, not below them. A warning under a table
    // is a warning read after the numbers have already been believed.
    for warning in run.conditions.warnings() {
        let _ = writeln!(out, "\n  !  {warning}");
    }

    let rows = build_rows(run, baseline, limits);
    let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(0).max(8);

    let _ = writeln!(out);
    let _ = writeln!(out, "{:<width$}  {:>11}  {:>11}   vs base", "", "median", "tail");

    for row in &rows {
        let (delta, mark) = match row.delta {
            None => (String::new(), ""),
            Some(delta) => (format_delta(delta), marker(delta.verdict)),
        };
        let _ = writeln!(
            out,
            "{:<width$}  {:>11}  {:>11}   {:>8} {}",
            row.id, row.median, row.tail, delta, mark,
        );
    }

    let _ = write!(out, "{}", summarize(&rows, limits));
    out
}

fn build_rows(run: &Run, baseline: Option<&Run>, limits: Thresholds) -> Vec<Row> {
    run.measurements
        .iter()
        .map(|measurement| {
            let delta = baseline
                .and_then(|base| base.measurement(&measurement.id))
                .filter(|_| measurement.unit.is_compared())
                .map(|before| {
                    Delta::compare(measurement.unit, &measurement.summary, &before.summary, limits)
                });

            Row {
                id: measurement.id.clone(),
                median: measurement.unit.format(measurement.summary.median),
                tail: measurement.unit.format(measurement.summary.tail),
                delta,
                // A benchmark whose own spread is a tenth of its value cannot
                // detect a ten percent change in anything. Worth knowing about
                // the benchmark, separately from what it measured.
                noisy_benchmark: measurement.summary.relative_noise() > 0.10,
            }
        })
        .collect()
}

fn format_delta(delta: Delta) -> String {
    match delta.verdict {
        // Nothing to compare against. An empty cell says that better than a
        // zero, which reads as "measured, and unchanged".
        Verdict::New => String::new(),
        _ => format!("{:+.1}%", delta.relative * 100.0),
    }
}

fn marker(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Slower => "SLOWER",
        Verdict::Faster => "faster",
        // Named rather than blank: a number that moved 8% and means nothing is
        // exactly the number somebody would otherwise chase for an hour.
        Verdict::Noisy => "(noise)",
        Verdict::Same | Verdict::New => "",
    }
}

fn summarize(rows: &[Row], limits: Thresholds) -> String {
    let verdicts = rows.iter().filter_map(|r| r.delta.map(|d| d.verdict));
    let regressions = verdicts.clone().filter(|v| *v == Verdict::Slower).count();
    let improvements = verdicts.filter(|v| *v == Verdict::Faster).count();
    let noisy = rows.iter().filter(|r| r.noisy_benchmark).count();

    let mut out = String::new();
    let _ = writeln!(out);

    match (regressions, improvements) {
        (0, 0) => {
            let _ = writeln!(
                out,
                "nothing moved past {:.0}%",
                limits.relative * 100.0
            );
        }
        (0, n) => {
            let _ = writeln!(out, "{n} faster, nothing slower");
        }
        (n, 0) => {
            let _ = writeln!(out, "{n} SLOWER past {:.0}%", limits.relative * 100.0);
        }
        (slower, faster) => {
            let _ = writeln!(out, "{slower} SLOWER, {faster} faster");
        }
    }

    if noisy > 0 {
        let _ = writeln!(
            out,
            "{noisy} benchmark{} too noisy to detect a 10% change \u{2014} \
             a quieter machine, or more samples",
            match noisy {
                1 => "",
                _ => "s",
            },
        );
    }

    out
}

/// How long ago, roughly, for a line nobody reads twice.
fn ago(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    match seconds {
        s if s < 0 => "in the future".into(),
        s if s < MINUTE => "just now".into(),
        s if s < HOUR => plural(s / MINUTE, "minute"),
        s if s < DAY => plural(s / HOUR, "hour"),
        s => plural(s / DAY, "day"),
    }
}

fn plural(count: i64, noun: &str) -> String {
    match count {
        1 => format!("1 {noun} ago"),
        n => format!("{n} {noun}s ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        history::SCHEMA,
        machine::{Conditions, Machine},
        runner::Measurement,
        stats::Summary,
    };

    fn machine() -> Machine {
        Machine {
            slug: "test".into(),
            cpu: "Test CPU".into(),
            model: "Test1,1".into(),
            cores: 8,
            memory: 0,
            os: "testOS".into(),
            arch: "aarch64".into(),
        }
    }

    fn measurement(id: &str, unit: Unit, samples: &[f64]) -> Measurement {
        Measurement {
            id: id.into(),
            unit,
            summary: Summary::of(samples.to_vec(), unit),
            batch: 1,
        }
    }

    fn run(at: i64, measurements: Vec<Measurement>) -> Run {
        Run {
            schema: SCHEMA,
            recorded_at: at,
            commit: "abc1234".into(),
            branch: "master".into(),
            dirty: false,
            corpus: 100_000,
            suites: vec!["store".into()],
            machine: machine(),
            conditions: Conditions::default(),
            measurements,
        }
    }

    #[test]
    fn a_first_run_says_it_has_nothing_to_compare_with() {
        let now = run(1_000, vec![measurement("store.list", Unit::Nanos, &[98_200_000.0; 9])]);
        let text = render(&now, None, Thresholds::default());

        assert!(text.contains("baseline none"), "{text}");
        assert!(text.contains("98.20ms"), "{text}");
        assert!(text.contains("nothing moved past 5%"), "{text}");
    }

    #[test]
    fn a_regression_is_named_and_counted() {
        let before = run(1_000, vec![measurement("html.blocks", Unit::Nanos, &[400_000.0; 9])]);
        let now = run(5_000, vec![measurement("html.blocks", Unit::Nanos, &[460_000.0; 9])]);

        let text = render(&now, Some(&before), Thresholds::default());
        assert!(text.contains("+15.0%"), "{text}");
        assert!(text.contains("SLOWER"), "{text}");
        assert!(text.contains("1 SLOWER past 5%"), "{text}");
    }

    #[test]
    fn an_improvement_is_reported_without_being_alarming() {
        let before = run(1_000, vec![measurement("store.list", Unit::Nanos, &[100.0; 9])]);
        let now = run(5_000, vec![measurement("store.list", Unit::Nanos, &[70.0; 9])]);

        let text = render(&now, Some(&before), Thresholds::default());
        assert!(text.contains("-30.0%"), "{text}");
        assert!(text.contains("faster"), "{text}");
        assert!(!text.contains("SLOWER"), "{text}");
    }

    #[test]
    fn a_throughput_that_fell_is_the_regression_not_the_rise() {
        let before = run(1_000, vec![measurement("store.ingest", Unit::PerSecond, &[31_000.0; 9])]);
        let now = run(5_000, vec![measurement("store.ingest", Unit::PerSecond, &[20_000.0; 9])]);

        let text = render(&now, Some(&before), Thresholds::default());
        assert!(text.contains("SLOWER"), "fewer messages per second is slower: {text}");
        assert!(text.contains("20,000/s"), "{text}");
    }

    #[test]
    fn a_move_inside_the_noise_is_labelled_rather_than_chased() {
        // The row that saves an afternoon: it moved 30%, and this benchmark
        // swings that much on its own.
        let wobbly = [50.0, 75.0, 100.0, 125.0, 150.0, 100.0, 60.0, 140.0];
        let scaled: Vec<f64> = wobbly.iter().map(|v| v * 1.3).collect();

        let before = run(1_000, vec![measurement("store.search", Unit::Nanos, &wobbly)]);
        let now = run(5_000, vec![measurement("store.search", Unit::Nanos, &scaled)]);

        let text = render(&now, Some(&before), Thresholds::default());
        assert!(text.contains("(noise)"), "{text}");
        assert!(!text.contains("SLOWER"), "a noisy move is not a regression: {text}");
        assert!(text.contains("too noisy"), "and the benchmark itself is called out: {text}");
    }

    #[test]
    fn a_count_is_shown_and_never_compared() {
        let before = run(1_000, vec![measurement("store.rows", Unit::Count, &[400.0])]);
        let now = run(5_000, vec![measurement("store.rows", Unit::Count, &[900.0])]);

        let text = render(&now, Some(&before), Thresholds::default());
        let row = text.lines().find(|l| l.starts_with("store.rows")).expect("the row");

        assert!(row.contains("900"), "{row}");
        assert!(!row.contains('%'), "a count has no delta: {row}");
    }

    #[test]
    fn a_benchmark_that_is_new_this_run_has_an_empty_delta() {
        let before = run(1_000, vec![measurement("old", Unit::Nanos, &[100.0; 9])]);
        let now = run(
            5_000,
            vec![
                measurement("old", Unit::Nanos, &[100.0; 9]),
                measurement("brand.new", Unit::Nanos, &[42.0; 9]),
            ],
        );

        let text = render(&now, Some(&before), Thresholds::default());
        let line = text.lines().find(|l| l.starts_with("brand.new")).expect("the new row");
        assert!(!line.contains('%'), "nothing to compare against: {line}");
    }

    #[test]
    fn the_conditions_are_stated_above_the_numbers() {
        // Below them is after they have been believed.
        let mut now = run(5_000, vec![measurement("store.list", Unit::Nanos, &[100.0; 9])]);
        now.conditions = Conditions { on_battery: true, speed_limit: None, load: None };

        let text = render(&now, None, Thresholds::default());
        let warning = text.find("battery").expect("the warning");
        let numbers = text.find("median").expect("the table");
        assert!(warning < numbers, "the warning has to come first:\n{text}");
    }

    #[test]
    fn elapsed_time_reads_the_way_people_say_it() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(50 * 3600), "2 days ago");
        // Clock skew between machines, or a history hand-edited out of order.
        assert_eq!(ago(-10), "in the future");
    }
}
