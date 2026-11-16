//! The history as a page you can look at.
//!
//! [`crate::report`] answers "did this commit make it slower" — one run against
//! one baseline, in a terminal. This answers the other question, the one you ask
//! after a week of changes: *where has it been going?* A column of percentages
//! cannot show that. Thirty small charts can.
//!
//! One self-contained file, written into `target/`. No server, no dependency on
//! being online, and nothing to install — the history is committed in the
//! repository and this turns it into something to look at, so it should work on
//! a machine that has just cloned it.
//!
//! The form is a grid of stat tiles: label, current value, the change since last
//! time, and a sparkline of every run before it. Each tile is its own chart with
//! its own scale, because a benchmark measured in nanoseconds and one measured
//! in messages per second share no axis — putting them on one would make
//! twenty-nine of the thirty a flat line along the bottom.

use serde::Serialize;

use crate::{history::Run, machine::Machine};

/// What the template holds where the data goes.
///
/// The fallback is part of it, so the template on disk is valid JavaScript and
/// so is the file that comes out — splicing in beside a leftover `null` would
/// leave `const DATA = {…} null;`, which is a syntax error and a blank page.
const PLACEHOLDER: &str = "/*__DATA__*/ null";

/// What the page is drawn from.
#[derive(Serialize)]
struct Page {
    machine: String,
    corpus: usize,
    note: String,
    runs: Vec<Sample>,
    benchmarks: Vec<Series>,
}

#[derive(Serialize)]
struct Sample {
    commit: String,
    dirty: bool,
    at: i64,
}

#[derive(Serialize)]
struct Series {
    id: String,
    /// Raw medians, positionally matching `runs`. `null` where a run did not
    /// measure this one — a suite that was skipped, or a benchmark added since.
    values: Vec<Option<f64>>,
    /// The same numbers as text.
    ///
    /// Formatted here rather than in the page, so there is one implementation
    /// of "how a duration is written" rather than a second one in JavaScript
    /// that drifts from it.
    formatted: Vec<Option<String>>,
    /// Which direction is an improvement. The page says "faster" or "slower" in
    /// words, and needs this to know which word applies.
    #[serde(rename = "higherIsBetter")]
    higher_is_better: bool,
}

/// Builds the page.
///
/// Only runs at the newest run's corpus size are drawn: a 10,000-message run
/// beside a 100,000-message one would read as a tenfold regression on every
/// line. The note on the page says so when any were left out.
pub fn render(runs: &[Run], machine: &Machine) -> String {
    let Some(newest) = runs.last() else {
        return include_str!("report.html").replace(PLACEHOLDER, &empty(machine));
    };

    let corpus = newest.corpus;
    let comparable: Vec<&Run> = runs.iter().filter(|run| run.corpus == corpus).collect();
    let skipped = runs.len() - comparable.len();

    // Every id any comparable run produced, in the order the newest run has
    // them — so a page's rows are ordered the way the suite runs rather than
    // alphabetically, and a benchmark that has since been removed still shows
    // its history at the end instead of vanishing.
    let mut ids: Vec<String> = newest.measurements.iter().map(|m| m.id.clone()).collect();
    for run in &comparable {
        for measurement in &run.measurements {
            if !ids.contains(&measurement.id) {
                ids.push(measurement.id.clone());
            }
        }
    }

    let benchmarks = ids
        .iter()
        .map(|id| {
            let found: Vec<Option<&crate::runner::Measurement>> =
                comparable.iter().map(|run| run.measurement(id)).collect();

            // The unit comes from the most recent run that measured it. A
            // benchmark whose unit changed has had its history invalidated
            // anyway, and the newest answer is the one to believe.
            let unit = found
                .iter()
                .rev()
                .flatten()
                .map(|m| m.unit)
                .next()
                .unwrap_or(crate::stats::Unit::Nanos);

            Series {
                id: id.clone(),
                values: found.iter().map(|m| m.map(|m| m.summary.median)).collect(),
                formatted: found
                    .iter()
                    .map(|m| m.map(|m| unit.format(m.summary.median)))
                    .collect(),
                higher_is_better: unit.higher_is_better(),
            }
        })
        .collect();

    let page = Page {
        machine: machine.describe(),
        corpus,
        note: note(&comparable, skipped),
        runs: comparable
            .iter()
            .map(|run| Sample {
                commit: run.commit.clone(),
                dirty: run.dirty,
                at: run.recorded_at,
            })
            .collect(),
        benchmarks,
    };

    let data = serde_json::to_string(&page).unwrap_or_else(|_| "null".into());
    include_str!("report.html").replace(PLACEHOLDER, &data)
}

/// What the reader should know before believing the charts.
fn note(comparable: &[&Run], skipped: usize) -> String {
    let mut parts = Vec::new();

    if skipped > 0 {
        parts.push(format!(
            "{skipped} run{} at a different corpus size {} hidden \u{2014} runs are only \
             comparable at the same size.",
            match skipped {
                1 => "",
                _ => "s",
            },
            match skipped {
                1 => "is",
                _ => "are",
            },
        ));
    }

    // A run taken on a machine that was busy or throttling is a run whose
    // numbers are soft, and a soft number in the middle of a trend looks
    // exactly like a regression that recovered.
    let unclean = comparable.iter().filter(|run| !run.conditions.are_clean()).count();
    if unclean > 0 {
        parts.push(format!(
            "{unclean} of these were measured on a busy or throttled machine and will read \
             slow.",
        ));
    }

    let dirty = comparable.iter().filter(|run| run.dirty).count();
    if dirty > 0 {
        parts.push(format!(
            "{dirty} were measured against an uncommitted working tree, marked * in the \
             table \u{2014} the code behind those points no longer exists.",
        ));
    }

    parts.join(" ")
}

fn empty(machine: &Machine) -> String {
    serde_json::to_string(&Page {
        machine: machine.describe(),
        corpus: 0,
        note: String::new(),
        runs: Vec::new(),
        benchmarks: Vec::new(),
    })
    .unwrap_or_else(|_| "null".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        history::SCHEMA,
        machine::Conditions,
        runner::Measurement,
        stats::{Summary, Unit},
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

    fn run(at: i64, corpus: usize, measurements: Vec<(&str, Unit, f64)>) -> Run {
        Run {
            schema: SCHEMA,
            recorded_at: at,
            commit: format!("c{at}"),
            branch: "master".into(),
            dirty: false,
            corpus,
            suites: vec!["store".into()],
            machine: machine(),
            conditions: Conditions::default(),
            measurements: measurements
                .into_iter()
                .map(|(id, unit, median)| Measurement {
                    id: id.into(),
                    unit,
                    summary: Summary::of(vec![median], unit),
                    batch: 1,
                })
                .collect(),
        }
    }

    /// The JSON the page is handed, pulled back out of the rendered file.
    fn data(html: &str) -> serde_json::Value {
        let line = html
            .lines()
            .find(|l| l.trim_start().starts_with("const DATA ="))
            .expect("the data line");
        let json = line
            .trim_start()
            .trim_start_matches("const DATA =")
            .trim()
            .trim_end_matches(';')
            .trim();
        serde_json::from_str(json).expect("valid JSON")
    }

    #[test]
    fn the_placeholder_leaves_no_trace_of_itself() {
        // The bug this exists for: splicing beside the template's fallback
        // left `const DATA = {…} null;` — a syntax error, and a page that
        // rendered nothing at all.
        let html = render(&[run(100, 1000, vec![("a", Unit::Nanos, 1.0)])], &machine());
        assert!(!html.contains("__DATA__"), "the placeholder is gone");
        assert!(!html.contains("} null;"), "and took the fallback with it");
    }

    #[test]
    fn a_machine_with_no_history_still_produces_a_page() {
        let html = render(&[], &machine());
        assert!(html.contains("<!doctype html>"));
        assert_eq!(data(&html)["runs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn every_run_becomes_a_point_and_every_benchmark_a_series() {
        let html = render(
            &[
                run(100, 1000, vec![("store.list", Unit::Nanos, 50.0)]),
                run(200, 1000, vec![("store.list", Unit::Nanos, 60.0)]),
            ],
            &machine(),
        );

        let d = data(&html);
        assert_eq!(d["runs"].as_array().unwrap().len(), 2);
        assert_eq!(d["benchmarks"].as_array().unwrap().len(), 1);
        assert_eq!(d["benchmarks"][0]["values"][0], 50.0);
        assert_eq!(d["benchmarks"][0]["values"][1], 60.0);
        assert_eq!(d["benchmarks"][0]["formatted"][1], "60ns");
    }

    #[test]
    fn runs_at_another_corpus_size_are_left_out_and_said_to_be() {
        // Drawing them would report a tenfold regression on every line.
        let html = render(
            &[
                run(100, 10_000, vec![("store.list", Unit::Nanos, 5.0)]),
                run(200, 100_000, vec![("store.list", Unit::Nanos, 50.0)]),
            ],
            &machine(),
        );

        let d = data(&html);
        assert_eq!(d["runs"].as_array().unwrap().len(), 1, "only the newest size");
        assert_eq!(d["corpus"], 100_000);
        assert!(d["note"].as_str().unwrap().contains("different corpus size"));
    }

    #[test]
    fn a_benchmark_missing_from_a_run_is_a_gap_rather_than_a_zero() {
        // A zero would draw as a cliff to the bottom of the chart and read as
        // the fastest result ever recorded.
        let html = render(
            &[
                run(100, 1000, vec![("store.list", Unit::Nanos, 50.0)]),
                run(200, 1000, vec![("store.list", Unit::Nanos, 60.0), ("new.one", Unit::Nanos, 9.0)]),
            ],
            &machine(),
        );

        let d = data(&html);
        let new = d["benchmarks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "new.one")
            .expect("the new benchmark");

        assert!(new["values"][0].is_null(), "not measured in the first run");
        assert_eq!(new["values"][1], 9.0);
    }

    #[test]
    fn throughput_says_which_way_is_better() {
        let html = render(
            &[run(100, 1000, vec![("store.ingest", Unit::PerSecond, 31_000.0)])],
            &machine(),
        );
        assert_eq!(data(&html)["benchmarks"][0]["higherIsBetter"], true);
    }

    #[test]
    fn a_soft_run_is_called_out_rather_than_drawn_as_a_regression() {
        let mut busy = run(200, 1000, vec![("store.list", Unit::Nanos, 90.0)]);
        busy.conditions = crate::machine::Conditions {
            on_battery: true,
            speed_limit: None,
            load: None,
        };

        let html = render(&[run(100, 1000, vec![("store.list", Unit::Nanos, 50.0)]), busy], &machine());
        assert!(data(&html)["note"].as_str().unwrap().contains("busy or throttled"));
    }

    #[test]
    fn the_newest_run_decides_the_order_of_the_tiles() {
        // So the page reads in the order the suite runs rather than
        // alphabetically, which would scatter `store.*` through `html.*`.
        let html = render(
            &[run(
                100,
                1000,
                vec![
                    ("store.ingest", Unit::PerSecond, 1.0),
                    ("html.blocks", Unit::Nanos, 2.0),
                    ("core.format", Unit::Nanos, 3.0),
                ],
            )],
            &machine(),
        );

        let d = data(&html);
        let ids: Vec<&str> = d["benchmarks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["store.ingest", "html.blocks", "core.format"]);
    }
}
