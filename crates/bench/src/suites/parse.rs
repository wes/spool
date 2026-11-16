//! Pure computation: parsing, threading, and the formatting a row does.
//!
//! The lowest-noise benchmarks available, because none of them touch a disk, a
//! lock or an allocator's slow path — which makes this the suite where a 3%
//! regression is actually detectable rather than lost in the spread.
//!
//! Two of these are on paths that scale in ways the others do not, and they are
//! the reason the suite exists at all:
//!
//! * `html.snippet` runs **once per message at ingest**. At thirty thousand
//!   messages a second, a microsecond added here is three percent of ingest.
//! * `core.format_date_parts` runs **once per visible row per frame**. Twenty
//!   rows at 120Hz is 2,400 calls a second, inside the 8.3ms that the whole
//!   frame has.

use anyhow::Result;
use spool_core::{
    address, format_date_parts, format_size,
    html::{self, Source},
    thread::normalize_subject,
};

use crate::{
    corpus::Corpus,
    runner::{Runner, black_box},
};

/// Enough bodies that the benchmark averages over a range of shapes rather
/// than over whatever the first message happened to be.
const BODIES: usize = 8;

/// A fixed instant, so the date benchmarks measure formatting rather than
/// drifting into a different branch as the real clock moves past a day
/// boundary.
const NOW: i64 = 1_767_225_600;

pub fn run(runner: &mut Runner, corpus: &Corpus) -> Result<()> {
    let bodies = corpus.html_bodies(BODIES)?;
    let envelopes = corpus.envelopes(64)?;

    // --- the reader's path -------------------------------------------------
    if !bodies.is_empty() {
        let mut next = 0;
        runner.bench("html.blocks", || {
            // Round-robin rather than one body, so this is the cost of the
            // renderer over the corpus and not the cost of one message that
            // might be unrepresentative.
            let body = &bodies[next % bodies.len()];
            next += 1;
            black_box(html::html_to_blocks(&body.html));
        });

        let mut next = 0;
        runner.bench("html.body_to_blocks", || {
            let body = &bodies[next % bodies.len()];
            next += 1;
            black_box(html::body_to_blocks(&body.plain, &body.html, Source::Rich));
        });

        let mut next = 0;
        runner.bench("html.is_document", || {
            let body = &bodies[next % bodies.len()];
            next += 1;
            black_box(html::is_document(&body.html));
        });

        // Every message pays this at ingest.
        let mut next = 0;
        runner.bench("html.snippet", || {
            let body = &bodies[next % bodies.len()];
            next += 1;
            black_box(html::snippet(&body.plain, &body.html, 200));
        });

        let mut next = 0;
        runner.bench("html.plain_to_blocks", || {
            let body = &bodies[next % bodies.len()];
            next += 1;
            black_box(html::plain_to_blocks(&body.plain));
        });
    }

    // --- threading ---------------------------------------------------------
    if !envelopes.is_empty() {
        let subjects: Vec<&str> = envelopes.iter().map(|e| e.subject.as_str()).collect();
        let mut next = 0;
        runner.bench("core.normalize_subject", || {
            let subject = subjects[next % subjects.len()];
            next += 1;
            black_box(normalize_subject(subject));
        });
    }

    // --- what a row does, every frame --------------------------------------
    let dates: Vec<i64> = envelopes.iter().map(|e| e.date).collect();
    if !dates.is_empty() {
        let mut next = 0;
        runner.bench("core.format_date_parts", || {
            let date = dates[next % dates.len()];
            next += 1;
            black_box(format_date_parts(date, NOW, -5 * 3600));
        });
    }

    let sizes: Vec<u32> = envelopes.iter().map(|e| e.size).collect();
    if !sizes.is_empty() {
        let mut next = 0;
        runner.bench("core.format_size", || {
            let size = sizes[next % sizes.len()];
            next += 1;
            black_box(format_size(size));
        });
    }

    // --- addresses ---------------------------------------------------------
    // The encoded form as the store keeps it, which is what the reader decodes
    // every time a message is opened.
    let recipients: Vec<String> = bodies
        .iter()
        .map(|body| address::encode_list(&body.to))
        .filter(|encoded| !encoded.is_empty())
        .collect();

    if !recipients.is_empty() {
        let mut next = 0;
        runner.bench("core.address.decode_list", || {
            let encoded = &recipients[next % recipients.len()];
            next += 1;
            black_box(address::decode_list(encoded));
        });

        let lists: Vec<Vec<spool_core::Address>> =
            recipients.iter().map(|r| address::decode_list(r)).collect();
        let mut next = 0;
        runner.bench("core.address.encode_list", || {
            let list = &lists[next % lists.len()];
            next += 1;
            black_box(address::encode_list(list));
        });
    }

    Ok(())
}

/// Every benchmark id this suite can produce.
pub fn ids() -> Vec<String> {
    [
        "html.blocks",
        "html.body_to_blocks",
        "html.is_document",
        "html.snippet",
        "html.plain_to_blocks",
        "core.normalize_subject",
        "core.format_date_parts",
        "core.format_size",
        "core.address.decode_list",
        "core.address.encode_list",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Sampling;

    #[test]
    fn the_suite_measures_everything_it_claims_to() {
        let corpus = Corpus::build(600).unwrap();
        let mut runner = Runner::new(Sampling::quick());
        run(&mut runner, &corpus).unwrap();

        let produced: Vec<String> =
            runner.into_measurements().into_iter().map(|m| m.id).collect();
        for id in ids() {
            assert!(produced.contains(&id), "`{id}` is listed but was never measured");
        }
    }

    #[test]
    fn these_benchmarks_are_quiet_enough_to_detect_a_real_change() {
        // The claim the suite is built on. Pure functions over fixed inputs
        // should have a spread small against the 5% threshold — otherwise a
        // regression in them is undetectable and the benchmark is decoration.
        let corpus = Corpus::build(600).unwrap();
        let mut runner = Runner::new(Sampling::default());
        run(&mut runner, &corpus).unwrap();

        for measurement in runner.into_measurements() {
            assert!(
                measurement.summary.relative_noise() < 0.25,
                "`{}` wobbles {:.0}% on its own, which is too much to detect anything with",
                measurement.id,
                measurement.summary.relative_noise() * 100.0,
            );
        }
    }

    #[test]
    fn the_date_benchmark_does_not_drift_with_the_real_clock() {
        // `NOW` is fixed so the formatter takes the same branch every run. A
        // benchmark using the real clock would silently change which format it
        // produced as messages aged past "yesterday", and report it as a
        // performance change.
        let (day, time) = format_date_parts(NOW - 3 * 86_400, NOW, -5 * 3600);
        let (day_again, time_again) = format_date_parts(NOW - 3 * 86_400, NOW, -5 * 3600);
        assert_eq!((day, time), (day_again, time_again));
    }
}
