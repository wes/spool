//! `spool-bench` — measure this commit, and say how it compares.
//!
//! ```text
//! spool-bench                       measure, compare, and record
//! spool-bench --quick               a rougher sweep, for iterating
//! spool-bench --corpus 250000       a bigger mailbox
//! spool-bench --suite parse         one suite
//! spool-bench --no-save             measure without writing history
//! spool-bench --baseline clean      compare against the last committed run
//! spool-bench --show                the recent history, and nothing else
//! spool-bench --list                every benchmark id
//! ```

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use spool_bench::{
    corpus::Corpus,
    history::{Baseline, History, Run},
    machine::{Conditions, Machine},
    report,
    runner::{Runner, Sampling},
    stats::{Thresholds, Unit},
    suites,
};

/// The default mailbox size.
///
/// A hundred thousand, because it is the size the README quotes and large
/// enough that the listing benchmarks are dominated by real work rather than by
/// the fixed cost of asking. A sweep at this size takes about a minute.
const DEFAULT_CORPUS: usize = 100_000;

struct Args {
    corpus: usize,
    suites: Vec<String>,
    sampling: Sampling,
    save: bool,
    baseline: Baseline,
    thresholds: Thresholds,
    show: Option<usize>,
    list: bool,
    /// Write the history out as a page, and measure nothing.
    report: Option<Option<std::path::PathBuf>>,
    open: bool,
    allow_debug: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            corpus: DEFAULT_CORPUS,
            suites: suites::HEADLESS.iter().map(|s| s.to_string()).collect(),
            sampling: Sampling::default(),
            save: true,
            baseline: Baseline::Previous,
            thresholds: Thresholds::default(),
            show: None,
            list: false,
            report: None,
            open: false,
            allow_debug: false,
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    // Holds the flag that `--report` looked at and decided was not its
    // argument, so the loop below sees it next.
    let mut pending: Option<String> = None;

    while let Some(arg) = pending.take().or_else(|| argv.next()) {
        match arg.as_str() {
            "--corpus" => {
                let n = argv.next().context("--corpus needs a message count")?;
                args.corpus = n.parse().context("--corpus needs a number")?;
            }
            "--suite" => {
                let names = argv.next().context("--suite needs a name")?;
                args.suites = names.split(',').map(|s| s.trim().to_string()).collect();

                for name in &args.suites {
                    if !suites::HEADLESS.contains(&name.as_str()) {
                        bail!("no suite called `{name}` — try {}", suites::HEADLESS.join(", "));
                    }
                }
            }
            "--quick" => args.sampling = Sampling::quick(),
            "--no-save" => args.save = false,
            "--baseline" => {
                let which = argv.next().context("--baseline needs a value")?;
                args.baseline = match which.as_str() {
                    "previous" | "last" => Baseline::Previous,
                    "clean" => Baseline::LastClean,
                    commit => Baseline::Commit(commit.to_string()),
                };
            }
            "--threshold" => {
                let percent: f64 = argv
                    .next()
                    .context("--threshold needs a percentage")?
                    .parse()
                    .context("--threshold needs a number")?;
                args.thresholds.relative = percent / 100.0;
            }
            "--show" => {
                args.show = Some(match argv.next() {
                    Some(n) => n.parse().context("--show needs a number of runs")?,
                    None => 10,
                });
            }
            "--report" => {
                // An optional path: `--report` on its own writes where the
                // help says it will, and `--report somewhere.html` does not
                // have to fight the next flag for its argument.
                let next = argv.next();
                args.report = Some(match next {
                    Some(value) if !value.starts_with("--") => Some(value.into()),
                    Some(flag) => {
                        pending = Some(flag);
                        None
                    }
                    None => None,
                });
            }
            "--open" => args.open = true,
            "--list" => args.list = true,
            "--allow-debug" => args.allow_debug = true,
            "--help" | "-h" => {
                println!("{}", include_str!("usage.txt"));
                std::process::exit(0);
            }
            other => bail!("unknown argument `{other}` — try --help"),
        }
    }

    Ok(args)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;

    if args.list {
        for id in suites::ids() {
            println!("{id}");
        }
        return Ok(());
    }

    let root = spool_bench::repo_root();
    let machine = Machine::detect();
    let history = History::for_machine(&root, &machine);

    if let Some(limit) = args.show {
        return show(&history, limit);
    }

    if let Some(path) = args.report {
        return write_report(&history, &machine, path, args.open, &root);
    }

    // A debug build measures a different program. Its numbers are three to ten
    // times slower and — worse for a history — slower by a different factor per
    // benchmark, so one debug run poisons every comparison that follows it.
    if cfg!(debug_assertions) && !args.allow_debug {
        bail!(
            "this is a debug build, and its numbers would not be comparable with anything.\n       \
             Run it with `--release`, or pass --allow-debug if you know why you want to."
        );
    }

    let conditions = Conditions::detect();
    for warning in conditions.warnings() {
        eprintln!("  !  {warning}");
    }

    eprintln!(
        "building a corpus of {} messages\u{2026}",
        Unit::Count.format(args.corpus as f64)
    );
    let corpus = Corpus::build(args.corpus)?;

    let mut runner = Runner::new(args.sampling).on_progress(|measurement| {
        eprintln!(
            "  {:<28} {:>11}",
            measurement.id,
            measurement.unit.format(measurement.summary.median)
        );
    });

    for suite in &args.suites {
        eprintln!("\n{suite}");
        match suite.as_str() {
            "store" => {
                suites::store::ingest(&mut runner, args.corpus)?;
                suites::store::run(&mut runner, &corpus)?;
            }
            "parse" => suites::parse::run(&mut runner, &corpus)?,
            other => bail!("no suite called `{other}`"),
        }
    }

    let run = Run::new(
        args.corpus,
        args.suites.clone(),
        conditions,
        runner.into_measurements(),
    );

    // The baseline is chosen from what was on disk *before* this run was
    // appended, so a run is never compared against itself.
    let previous = history.load()?;
    let baseline = args.baseline.find(&previous, args.corpus);

    println!("\n{}", report::render(&run, baseline, args.thresholds));

    if args.save {
        history.append(&run)?;
        println!("recorded in {}", relative(history.path(), &root).display());
    } else {
        println!("not recorded (--no-save)");
    }

    Ok(())
}

/// Writes the history out as a page.
fn write_report(
    history: &History,
    machine: &Machine,
    path: Option<std::path::PathBuf>,
    open: bool,
    root: &std::path::Path,
) -> Result<()> {
    let runs = history.load()?;
    let path = path.unwrap_or_else(|| root.join("target/bench-report.html"));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, spool_bench::page::render(&runs, machine))
        .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "{} run{} \u{2192} {}",
        runs.len(),
        match runs.len() {
            1 => "",
            _ => "s",
        },
        path.display()
    );

    if open {
        // `open` rather than a URL: the file is local, and handing it to the
        // browser by path avoids inventing a scheme for it.
        let _ = std::process::Command::new("open").arg(&path).status();
    }

    Ok(())
}

/// Prints the tail of the history, without measuring anything.
fn show(history: &History, limit: usize) -> Result<()> {
    let runs = history.load()?;
    if runs.is_empty() {
        println!("no history yet at {}", history.path().display());
        return Ok(());
    }

    println!("{:<10} {:<24} {:>9}  suites", "commit", "when", "corpus");
    for run in runs.iter().rev().take(limit).rev() {
        println!(
            "{:<10} {:<24} {:>9}  {}",
            run.label(),
            stamp(run.recorded_at),
            Unit::Count.format(run.corpus as f64),
            run.suites.join(", "),
        );
    }

    Ok(())
}

/// A unix timestamp as a date, using the app's own formatter so the two agree.
fn stamp(seconds: i64) -> String {
    spool_core::format_date_long(seconds, 0)
}

/// A path relative to the repository, for printing.
fn relative<'a>(path: &'a std::path::Path, root: &std::path::Path) -> &'a std::path::Path {
    path.strip_prefix(root).unwrap_or(path)
}
