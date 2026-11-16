//! Turning code into samples.
//!
//! Three ways to measure something, because three kinds of thing are being
//! measured and one strategy for all of them would be wrong twice:
//!
//! * [`Runner::bench`] — cheap and repeatable. Parsing a message, formatting a
//!   date. Too fast to time individually, so it is timed in batches sized at
//!   run time from what a warmup says it costs, and the batch is divided out.
//! * [`Runner::bench_each`] — expensive, with state that must be fresh.
//!   Ingesting a hundred thousand messages into a database that has to be empty
//!   when it starts. Setup is not timed.
//! * [`Runner::record`] — not a duration at all. Bytes on disk, rows returned.
//!
//! What all three have in common is that they hand back nothing but samples.
//! The judgement of what the samples mean is [`crate::stats`]'s, and keeping
//! those apart is what makes the judgement testable without running a
//! benchmark.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::stats::{Summary, Unit};

/// Prevents the optimizer from deleting the work being measured.
///
/// A benchmark whose result is unused is a benchmark of nothing, and at
/// `opt-level = 3` that is exactly what it becomes. Every closure passed to a
/// runner should pass its result through this.
pub use std::hint::black_box;

/// How hard to sample.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    /// Run the code for at least this long before believing anything it does.
    ///
    /// Warmup is not superstition: the first pass pays for page faults, a cold
    /// branch predictor, an empty page cache and — for anything touching
    /// SQLite — a cold statement cache. A running app paid those once at
    /// startup, so a benchmark that counts them is measuring a state the app is
    /// never in.
    pub warmup: Duration,
    /// How long each sample should take, which decides the batch size.
    ///
    /// Long enough to swamp the clock's own resolution and the loop overhead,
    /// short enough that a scheduler interruption lands in one sample instead
    /// of all of them.
    pub target_sample: Duration,
    /// How many samples to take when there is time for them.
    pub samples: usize,
    /// The most time any single benchmark may spend.
    ///
    /// A cap rather than a promise. Without it, one benchmark that got slower
    /// makes the whole suite take longer to tell you so.
    pub max_total: Duration,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            warmup: Duration::from_millis(100),
            target_sample: Duration::from_millis(20),
            samples: 25,
            max_total: Duration::from_secs(5),
        }
    }
}

impl Sampling {
    /// A quicker, rougher sweep — for iterating on the harness itself, or for
    /// a look while something else is running on the machine.
    pub fn quick() -> Self {
        Self {
            warmup: Duration::from_millis(20),
            target_sample: Duration::from_millis(5),
            samples: 7,
            max_total: Duration::from_secs(1),
        }
    }
}

/// One benchmark's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    /// A stable, dotted name: `store.search.deploy`. Stable is the operative
    /// word — it is the key a run is joined to its history by, so renaming one
    /// starts its history over.
    pub id: String,
    pub unit: Unit,
    #[serde(flatten)]
    pub summary: Summary,
    /// How many iterations went into each sample. `1` for anything measured
    /// individually.
    pub batch: usize,
}

impl Measurement {
    pub fn value(&self) -> f64 {
        self.summary.median
    }
}

/// Collects measurements, and says what it is doing while it does.
pub struct Runner {
    sampling: Sampling,
    measurements: Vec<Measurement>,
    /// Called as each benchmark finishes, so a sweep that takes a minute is
    /// not a minute of silence.
    progress: Box<dyn FnMut(&Measurement)>,
}

impl Runner {
    pub fn new(sampling: Sampling) -> Self {
        Self { sampling, measurements: Vec::new(), progress: Box::new(|_| {}) }
    }

    pub fn on_progress(mut self, f: impl FnMut(&Measurement) + 'static) -> Self {
        self.progress = Box::new(f);
        self
    }

    pub fn into_measurements(self) -> Vec<Measurement> {
        self.measurements
    }

    /// Times a cheap, repeatable operation, batching it automatically.
    pub fn bench(&mut self, id: impl Into<String>, mut f: impl FnMut()) {
        // Warm up, and use the warmup to find out what one iteration costs.
        let warm_started = Instant::now();
        let mut warm_iterations: u64 = 0;
        while warm_started.elapsed() < self.sampling.warmup || warm_iterations == 0 {
            f();
            warm_iterations += 1;
        }
        let per_iteration =
            warm_started.elapsed().as_secs_f64() / warm_iterations.max(1) as f64;

        // A batch that takes about `target_sample`. Clamped at the bottom
        // because an operation slower than a whole sample still gets measured
        // one iteration at a time, and at the top so a nanosecond-scale
        // operation does not produce a batch of a billion.
        let batch = match per_iteration > 0.0 {
            true => (self.sampling.target_sample.as_secs_f64() / per_iteration) as u64,
            false => u64::MAX,
        }
        .clamp(1, 5_000_000) as usize;

        let mut samples = Vec::with_capacity(self.sampling.samples);
        let started = Instant::now();
        loop {
            let sample_started = Instant::now();
            for _ in 0..batch {
                f();
            }
            let elapsed = sample_started.elapsed();
            samples.push(elapsed.as_nanos() as f64 / batch as f64);

            // Checked after taking one, so every benchmark produces at least
            // one sample however long it turned out to take.
            if samples.len() >= self.sampling.samples || started.elapsed() >= self.sampling.max_total
            {
                break;
            }
        }

        self.push(id, Unit::Nanos, samples, batch);
    }

    /// Times an expensive operation that needs fresh state each time.
    ///
    /// `setup` runs outside the clock. `samples` is given rather than derived,
    /// because the caller knows what one of these costs and the harness would
    /// only find out by spending it.
    pub fn bench_each<S>(
        &mut self,
        id: impl Into<String>,
        samples: usize,
        mut setup: impl FnMut() -> S,
        mut f: impl FnMut(&mut S),
    ) {
        let mut collected = Vec::with_capacity(samples);
        for _ in 0..samples.max(1) {
            let mut state = setup();
            let started = Instant::now();
            f(&mut state);
            collected.push(started.elapsed().as_nanos() as f64);
        }
        self.push(id, Unit::Nanos, collected, 1);
    }

    /// The same, reported as work per second.
    ///
    /// `items` is how much work one call does — the message count for an
    /// ingest — and the samples become items per second so the number reads the
    /// way the README quotes it.
    pub fn bench_throughput<S>(
        &mut self,
        id: impl Into<String>,
        items: usize,
        samples: usize,
        mut setup: impl FnMut() -> S,
        mut f: impl FnMut(&mut S),
    ) {
        let mut collected = Vec::with_capacity(samples);
        for _ in 0..samples.max(1) {
            let mut state = setup();
            let started = Instant::now();
            f(&mut state);
            let seconds = started.elapsed().as_secs_f64();
            collected.push(match seconds > 0.0 {
                true => items as f64 / seconds,
                false => 0.0,
            });
        }
        self.push(id, Unit::PerSecond, collected, 1);
    }

    /// Records a number that was observed rather than timed.
    pub fn record(&mut self, id: impl Into<String>, unit: Unit, value: f64) {
        self.push(id, unit, vec![value], 1);
    }

    fn push(&mut self, id: impl Into<String>, unit: Unit, samples: Vec<f64>, batch: usize) {
        let measurement = Measurement {
            id: id.into(),
            unit,
            summary: Summary::of(samples, unit),
            batch,
        };
        (self.progress)(&measurement);
        self.measurements.push(measurement);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> Runner {
        Runner::new(Sampling {
            warmup: Duration::from_millis(2),
            target_sample: Duration::from_millis(1),
            samples: 3,
            max_total: Duration::from_millis(200),
        })
    }

    #[test]
    fn a_cheap_operation_is_batched_rather_than_timed_one_at_a_time() {
        let mut runner = quick();
        runner.bench("adding", || {
            black_box(black_box(2_u64) + black_box(2));
        });

        let measured = runner.into_measurements();
        assert_eq!(measured.len(), 1);
        assert_eq!(measured[0].id, "adding");

        // An addition is a nanosecond or so, which is below the clock's own
        // resolution — so it has to have been batched for the number to mean
        // anything at all.
        assert!(measured[0].batch > 1, "batch was {}", measured[0].batch);
        assert!(measured[0].summary.samples >= 1);
    }

    #[test]
    fn something_slower_than_a_sample_still_gets_measured_once_per_iteration() {
        let mut runner = quick();
        runner.bench("sleeping", || {
            std::thread::sleep(Duration::from_millis(3));
        });

        let measured = runner.into_measurements();
        assert_eq!(measured[0].batch, 1, "a batch of one is the floor");
        assert!(
            measured[0].summary.median >= 2_000_000.0,
            "it should have measured about 3ms, got {}",
            measured[0].summary.median
        );
    }

    #[test]
    fn every_benchmark_produces_at_least_one_sample_however_slow_it_is() {
        // `max_total` is a cap, not a promise to fit inside it. A benchmark
        // slower than the whole budget must still report a number rather than
        // summarizing an empty list, which would panic.
        let mut runner = Runner::new(Sampling {
            warmup: Duration::from_millis(1),
            target_sample: Duration::from_millis(1),
            samples: 10,
            max_total: Duration::from_millis(1),
        });
        runner.bench("slow", || std::thread::sleep(Duration::from_millis(5)));

        let measured = runner.into_measurements();
        assert_eq!(measured[0].summary.samples, 1);
    }

    #[test]
    fn setup_is_not_counted_against_the_thing_being_measured() {
        let mut runner = quick();
        runner.bench_each(
            "with-setup",
            3,
            || std::thread::sleep(Duration::from_millis(5)),
            |_| std::thread::sleep(Duration::from_millis(1)),
        );

        let measured = runner.into_measurements();
        // If setup were timed this would be at least 6ms.
        assert!(
            measured[0].summary.median < 4_000_000.0,
            "setup leaked into the measurement: {}",
            measured[0].summary.median
        );
    }

    #[test]
    fn throughput_is_items_over_the_time_they_took() {
        let mut runner = quick();
        runner.bench_throughput("ingesting", 1_000, 3, || (), |_| {
            std::thread::sleep(Duration::from_millis(10))
        });

        let measured = runner.into_measurements();
        assert_eq!(measured[0].unit, Unit::PerSecond);
        // 1,000 items in ~10ms is ~100,000/s. Loose bounds: this is a timing
        // test on a shared machine, and the point is the arithmetic, not the
        // sleep's precision.
        let rate = measured[0].summary.median;
        assert!((10_000.0..1_000_000.0).contains(&rate), "{rate}");
    }

    #[test]
    fn an_observed_number_is_recorded_without_being_timed() {
        let mut runner = quick();
        runner.record("on-disk", Unit::Bytes, 287_309_824.0);

        let measured = runner.into_measurements();
        assert_eq!(measured[0].unit, Unit::Bytes);
        assert_eq!(measured[0].summary.median, 287_309_824.0);
        assert_eq!(measured[0].summary.mad, 0.0, "one observation has no spread");
    }

    #[test]
    fn progress_is_reported_as_each_one_finishes() {
        use std::{cell::RefCell, rc::Rc};

        let seen = Rc::new(RefCell::new(Vec::new()));
        let recorder = seen.clone();

        let mut runner = quick().on_progress(move |m| recorder.borrow_mut().push(m.id.clone()));
        runner.record("one", Unit::Count, 1.0);
        runner.record("two", Unit::Count, 2.0);

        assert_eq!(*seen.borrow(), ["one", "two"]);
    }
}
