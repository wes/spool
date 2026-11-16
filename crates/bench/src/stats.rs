//! The statistics, and the decision about what counts as a change.
//!
//! Everything here is robust rather than parametric — median instead of mean,
//! median absolute deviation instead of standard deviation. That is not
//! fastidiousness. A benchmark on a laptop is a distribution with a long right
//! tail: the scheduler, a background process, a thermal event. A mean and a
//! standard deviation are both dragged by exactly those samples, so a run that
//! was interrupted once reads as uniformly slower. A median ignores them, which
//! is the correct response to a sample that measured something other than the
//! code.
//!
//! The other half of this module is deciding whether a difference means
//! anything, which is the part a benchmark system lives or dies on. A tool that
//! reports every 2% wobble as a regression is one whose output nobody reads by
//! the second week.

use serde::{Deserialize, Serialize};

/// What a number means, which decides both how it prints and which direction
/// is an improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    /// A duration in nanoseconds. Lower is better.
    Nanos,
    /// Work per second. **Higher is better**, which is the whole reason this
    /// enum exists rather than everything being a duration.
    PerSecond,
    /// A size in bytes. Lower is better.
    Bytes,
    /// A plain count — rows returned, messages matched. Not a performance
    /// number at all; carried so a run records what the workload actually did,
    /// and never compared.
    Count,
}

impl Unit {
    /// Whether a larger number is a better one.
    pub fn higher_is_better(self) -> bool {
        matches!(self, Unit::PerSecond)
    }

    /// Whether a change in this number is worth reporting as a change.
    ///
    /// A count is a description of the workload — "the search matched 412
    /// rows" — and a corpus that produces a different number of matches is a
    /// different corpus, not a slower one.
    pub fn is_compared(self) -> bool {
        !matches!(self, Unit::Count)
    }

    pub fn format(self, value: f64) -> String {
        match self {
            Unit::Nanos => format_duration(value),
            Unit::PerSecond => format!("{}/s", thousands(value)),
            Unit::Bytes => format_bytes(value),
            Unit::Count => thousands(value),
        }
    }
}

/// Nanoseconds, at three significant figures and a unit that suits the size.
///
/// Deliberately not the app's own `format_duration`: this one keeps a fixed
/// number of significant figures so a column of them lines up and a 1% change
/// is visible, where the status bar's rounds for glanceability.
pub fn format_duration(nanos: f64) -> String {
    match nanos {
        n if n < 1_000.0 => format!("{n:.0}ns"),
        n if n < 1_000_000.0 => format!("{:.1}\u{b5}s", n / 1_000.0),
        n if n < 1_000_000_000.0 => format!("{:.2}ms", n / 1_000_000.0),
        n => format!("{:.2}s", n / 1_000_000_000.0),
    }
}

pub fn format_bytes(bytes: f64) -> String {
    const KB: f64 = 1024.0;
    match bytes {
        b if b < KB => format!("{b:.0}B"),
        b if b < KB * KB => format!("{:.0}KB", b / KB),
        b if b < KB * KB * KB => format!("{:.1}MB", b / (KB * KB)),
        b => format!("{:.2}GB", b / (KB * KB * KB)),
    }
}

fn thousands(value: f64) -> String {
    let rounded = value.round() as i64;
    let digits = rounded.abs().to_string();
    let mut out = String::new();

    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    match rounded < 0 {
        true => format!("-{out}"),
        false => out,
    }
}

/// The summary of one benchmark's samples.
///
/// The raw samples are deliberately *not* kept in the history file. A hundred
/// runs of forty benchmarks at twenty samples each is eighty thousand numbers
/// nobody will ever read, and the four below are what every comparison uses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub median: f64,
    /// The pessimistic tail — the number that corresponds to a visible
    /// stutter, which a median will happily hide.
    ///
    /// Which end of the distribution that is depends on the unit, which is why
    /// building one requires knowing it. For a duration the bad tail is the
    /// 95th percentile, the slow end. For a throughput it is the *5th*: the
    /// samples where the least work got done. Calling both of them `p95` is
    /// how a report ends up quoting a throughput's best moment as if it were
    /// its worst.
    pub tail: f64,
    /// Median absolute deviation: the spread, measured the same robust way as
    /// the middle. This is what decides whether a difference is signal.
    pub mad: f64,
    /// How many samples went into it.
    pub samples: usize,
}

impl Summary {
    /// Summarizes samples. Sorts them, so it takes them by value.
    ///
    /// Panics on an empty slice, which can only happen if a benchmark was
    /// defined with zero iterations — a bug in the harness rather than a
    /// condition to handle.
    pub fn of(mut samples: Vec<f64>, unit: Unit) -> Self {
        assert!(!samples.is_empty(), "a benchmark produced no samples");
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let median = percentile_sorted(&samples, 0.5);
        let deviations: Vec<f64> = samples.iter().map(|s| (s - median).abs()).collect();

        // The bad end, whichever end that is for this unit.
        let tail = match unit.higher_is_better() {
            true => percentile_sorted(&samples, 0.05),
            false => percentile_sorted(&samples, 0.95),
        };

        Self { median, tail, mad: median_of(deviations), samples: samples.len() }
    }

    /// The spread as a fraction of the middle, which is how noisy this
    /// benchmark is in terms that survive being compared across machines.
    pub fn relative_noise(&self) -> f64 {
        match self.median.abs() < f64::EPSILON {
            true => 0.0,
            false => self.mad / self.median,
        }
    }
}

fn median_of(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    percentile_sorted(&values, 0.5)
}

/// Linear-interpolated percentile of an already-sorted slice.
fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    match sorted.len() {
        0 => 0.0,
        1 => sorted[0],
        n => {
            let position = q * (n - 1) as f64;
            let lower = position.floor() as usize;
            let upper = position.ceil() as usize;
            match lower == upper {
                true => sorted[lower],
                false => {
                    let weight = position - lower as f64;
                    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
                }
            }
        }
    }
}

/// How a measurement compares to the one it is being held against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Faster,
    Slower,
    /// Inside the threshold — the same number as far as anyone cares.
    Same,
    /// Outside the threshold but inside the noise. The benchmark moved and the
    /// benchmark also wobbles this much on its own, so the move means nothing.
    ///
    /// Reported rather than folded into `Same`, because a benchmark that is
    /// permanently too noisy to say anything is itself a thing worth fixing.
    Noisy,
    /// Nothing to compare against.
    New,
}

impl Verdict {
    pub fn is_regression(self) -> bool {
        matches!(self, Verdict::Slower)
    }
}

/// How much a number has to move, and how far above its own noise it has to
/// move, before the tool will say anything about it.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Relative change worth mentioning. 0.05 is five percent.
    pub relative: f64,
    /// How many robust standard deviations the change has to clear.
    ///
    /// Three is the usual place to draw this. Below it, a benchmark that
    /// happens to be noisy reports a regression on every other run, and the
    /// report stops being read.
    pub sigma: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { relative: 0.05, sigma: 3.0 }
    }
}

/// One number, held against its baseline.
#[derive(Debug, Clone, Copy)]
pub struct Delta {
    /// Signed fraction: +0.12 is twelve percent larger, whatever larger means
    /// for the unit.
    pub relative: f64,
    pub verdict: Verdict,
}

impl Delta {
    pub fn compare(unit: Unit, now: &Summary, before: &Summary, limits: Thresholds) -> Self {
        if !unit.is_compared() || before.median.abs() < f64::EPSILON {
            return Self { relative: 0.0, verdict: Verdict::New };
        }

        let relative = (now.median - before.median) / before.median;
        if relative.abs() < limits.relative {
            return Self { relative, verdict: Verdict::Same };
        }

        // 1.4826 scales a median absolute deviation into the same units as a
        // standard deviation for normally distributed data, which is the
        // constant that makes a sigma threshold mean what people expect. Both
        // sides' spread counts: a change is only interesting if it is large
        // against the noise of the measurement it came from *and* the one it
        // is held against.
        let spread = 1.4826 * (before.mad + now.mad).max(f64::EPSILON);
        let sigmas = (now.median - before.median).abs() / spread;

        if sigmas < limits.sigma {
            return Self { relative, verdict: Verdict::Noisy };
        }

        let bigger_is_better = unit.higher_is_better();
        let got_bigger = relative > 0.0;
        Self {
            relative,
            verdict: match got_bigger == bigger_is_better {
                true => Verdict::Faster,
                false => Verdict::Slower,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(values: &[f64]) -> Summary {
        Summary::of(values.to_vec(), Unit::Nanos)
    }

    #[test]
    fn the_middle_of_a_sample_is_its_median() {
        assert_eq!(summary(&[1.0, 2.0, 3.0]).median, 2.0);
        assert_eq!(summary(&[1.0, 2.0, 3.0, 4.0]).median, 2.5);
        assert_eq!(summary(&[7.0]).median, 7.0);
    }

    #[test]
    fn one_interrupted_sample_does_not_move_the_answer() {
        // The case the whole module is built around: nineteen good samples and
        // one that caught a context switch. A mean would report this benchmark
        // as five times slower than it is.
        let mut values = vec![100.0; 19];
        values.push(10_000.0);

        let summarized = summary(&values);
        assert_eq!(summarized.median, 100.0, "the outlier is ignored");

        let mean: f64 = values.iter().sum::<f64>() / values.len() as f64;
        assert!(mean > 500.0, "a mean would have been dragged to {mean}");
    }

    #[test]
    fn the_tail_is_reported_as_well_as_the_middle() {
        // The tail is where a stutter lives. A benchmark whose median is fine
        // and whose tail is awful is a benchmark with a problem.
        let mut values = vec![100.0; 95];
        values.extend(std::iter::repeat_n(900.0, 5));

        let summarized = summary(&values);
        assert_eq!(summarized.median, 100.0);
        assert!(summarized.tail > 100.0, "the slow end shows: {}", summarized.tail);
    }

    #[test]
    fn the_bad_tail_of_a_throughput_is_its_slow_end() {
        // The mistake this guards against: for messages-per-second, the
        // interesting tail is where the *fewest* messages went through. Taking
        // a 95th percentile of a throughput quotes its best moment as its
        // worst, which is a benchmark that flatters itself.
        let mut values = vec![30_000.0; 95];
        values.extend(std::iter::repeat_n(4_000.0, 5));

        let throughput = Summary::of(values.clone(), Unit::PerSecond);
        assert_eq!(throughput.median, 30_000.0);
        assert!(
            throughput.tail < throughput.median,
            "the five slow samples have to show: {}",
            throughput.tail,
        );

        // The very same numbers read as durations take the other end, and so
        // find nothing — there is no *high* outlier here.
        let duration = Summary::of(values, Unit::Nanos);
        assert_eq!(duration.tail, 30_000.0);
        assert!(duration.tail > throughput.tail, "opposite ends of one distribution");
    }

    #[test]
    fn a_steady_benchmark_has_no_spread() {
        let steady = summary(&[50.0; 20]);
        assert_eq!(steady.mad, 0.0);
        assert_eq!(steady.relative_noise(), 0.0);
    }

    #[test]
    fn a_small_change_is_not_a_change() {
        let before = summary(&[100.0; 20]);
        let now = summary(&[102.0; 20]);

        let delta = Delta::compare(Unit::Nanos, &now, &before, Thresholds::default());
        assert_eq!(delta.verdict, Verdict::Same, "2% is inside the 5% threshold");
    }

    #[test]
    fn a_large_change_on_a_steady_benchmark_is_a_regression() {
        let before = summary(&[100.0; 20]);
        let now = summary(&[130.0; 20]);

        let delta = Delta::compare(Unit::Nanos, &now, &before, Thresholds::default());
        assert_eq!(delta.verdict, Verdict::Slower);
        assert!((delta.relative - 0.30).abs() < 1e-9, "{}", delta.relative);
        assert!(delta.verdict.is_regression());
    }

    #[test]
    fn the_same_change_on_a_noisy_benchmark_says_nothing() {
        // This is the test that keeps the tool worth reading. The medians are
        // 30% apart, but the benchmark routinely swings further than that on
        // its own, so the difference is not evidence of anything.
        let before = summary(&[50.0, 75.0, 100.0, 125.0, 150.0, 100.0, 60.0, 140.0]);
        let now = summary(&[65.0, 97.0, 130.0, 163.0, 195.0, 130.0, 78.0, 182.0]);

        let delta = Delta::compare(Unit::Nanos, &now, &before, Thresholds::default());
        assert_eq!(delta.verdict, Verdict::Noisy, "{delta:?}");
    }

    #[test]
    fn faster_means_a_smaller_duration_and_a_larger_throughput() {
        let slow = summary(&[100.0; 20]);
        let fast = summary(&[70.0; 20]);

        // A duration that fell is an improvement.
        assert_eq!(
            Delta::compare(Unit::Nanos, &fast, &slow, Thresholds::default()).verdict,
            Verdict::Faster,
        );
        // A throughput that fell is a regression — the reason `Unit` knows
        // which way round it runs.
        assert_eq!(
            Delta::compare(Unit::PerSecond, &fast, &slow, Thresholds::default()).verdict,
            Verdict::Slower,
        );
        assert_eq!(
            Delta::compare(Unit::PerSecond, &slow, &fast, Thresholds::default()).verdict,
            Verdict::Faster,
        );
    }

    #[test]
    fn a_count_is_described_and_never_judged() {
        // "The search matched 412 rows" is a fact about the corpus. A corpus
        // that matches a different number is a different corpus, not a
        // regression.
        let before = summary(&[400.0; 5]);
        let now = summary(&[900.0; 5]);

        let delta = Delta::compare(Unit::Count, &now, &before, Thresholds::default());
        assert_eq!(delta.verdict, Verdict::New);
        assert!(!Unit::Count.is_compared());
    }

    #[test]
    fn durations_are_formatted_at_the_scale_they_land_in() {
        assert_eq!(format_duration(412.0), "412ns");
        assert_eq!(format_duration(1_500.0), "1.5\u{b5}s");
        assert_eq!(format_duration(5_310_000.0), "5.31ms");
        assert_eq!(format_duration(2_500_000_000.0), "2.50s");
    }

    #[test]
    fn throughput_is_grouped_so_a_column_of_it_can_be_read() {
        assert_eq!(Unit::PerSecond.format(31_402.0), "31,402/s");
        assert_eq!(Unit::PerSecond.format(950.0), "950/s");
        assert_eq!(Unit::PerSecond.format(1_000_000.0), "1,000,000/s");
        assert_eq!(Unit::Count.format(412.0), "412");
    }

    #[test]
    fn sizes_are_formatted_like_sizes() {
        assert_eq!(format_bytes(512.0), "512B");
        assert_eq!(format_bytes(2048.0), "2KB");
        assert_eq!(format_bytes(287_309_824.0), "274.0MB");
    }
}
