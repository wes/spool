//! What was measured, when, and on what — kept in the repository.
//!
//! One JSON object per line, one file per machine, committed alongside the
//! code. That is the whole storage design, and each third of it is deliberate.
//!
//! **JSON Lines** because appending is the only write that ever happens, and a
//! format where appending is `>>` cannot corrupt what is already there. A
//! benchmark history that can be damaged by a run that crashed halfway is one
//! nobody trusts.
//!
//! **One file per machine** because numbers from two machines are not
//! comparable. Keeping them in one file would make the mistake possible; keeping
//! them apart makes it awkward, which is the right amount of hard.
//!
//! **In the repository** because the question is "did this change make it
//! slower", and that question is asked against history. A results directory in
//! `~` answers it only for whoever is sitting at that machine, and only until
//! they clear it out.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    machine::{Conditions, Machine},
    runner::Measurement,
};

/// Bumped when a field changes meaning rather than merely being added.
///
/// A reader that finds a schema it does not know skips the record instead of
/// interpreting it wrongly — an old run misread as a new one is a phantom
/// regression that costs an afternoon.
pub const SCHEMA: u32 = 1;

/// Everything one sweep produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub schema: u32,
    /// Unix seconds.
    pub recorded_at: i64,
    /// Short hash of the commit this was measured at.
    pub commit: String,
    pub branch: String,
    /// Whether the working tree had uncommitted changes.
    ///
    /// Recorded because a dirty run is not reproducible: the code it measured
    /// no longer exists anywhere. Useful to take, worth marking, and not what
    /// you want to be comparing against next week.
    pub dirty: bool,
    /// How many messages the corpus held. Runs at different corpus sizes are
    /// different benchmarks and are never compared.
    pub corpus: usize,
    /// Which suites ran, so a partial sweep is not mistaken for a full one.
    pub suites: Vec<String>,
    pub machine: Machine,
    pub conditions: Conditions,
    pub measurements: Vec<Measurement>,
}

impl Run {
    pub fn new(
        corpus: usize,
        suites: Vec<String>,
        conditions: Conditions,
        measurements: Vec<Measurement>,
    ) -> Self {
        let git = Git::read();
        Self {
            schema: SCHEMA,
            recorded_at: now_seconds(),
            commit: git.commit,
            branch: git.branch,
            dirty: git.dirty,
            corpus,
            suites,
            machine: Machine::detect(),
            conditions,
            measurements,
        }
    }

    pub fn measurement(&self, id: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.id == id)
    }

    /// How this run identifies itself in a report.
    pub fn label(&self) -> String {
        match self.dirty {
            true => format!("{} (dirty)", self.commit),
            false => self.commit.clone(),
        }
    }
}

/// Which earlier run to hold this one against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Baseline {
    /// The most recent previous run at the same corpus size, whatever state it
    /// was in. The default, because the everyday question is "slower than last
    /// time I looked?"
    Previous,
    /// The most recent run from a clean working tree — a reference point that
    /// corresponds to code that still exists. What to use when a long run of
    /// dirty experiments has made `Previous` meaningless.
    LastClean,
    /// A specific commit, by prefix.
    Commit(String),
}

impl Baseline {
    /// Picks it out of a history, oldest-first.
    ///
    /// `runs` must not include the run being compared — a baseline is
    /// something to hold a new measurement against, and a number held against
    /// itself always looks unchanged.
    pub fn find<'a>(&self, runs: &'a [Run], corpus: usize) -> Option<&'a Run> {
        let mut comparable = runs.iter().filter(|run| run.corpus == corpus);

        // `next_back` rather than `last`: the history is oldest-first and the
        // answer is always at the end of it, so walking the whole thing to
        // find out would be the wrong direction to read from.
        match self {
            Self::Previous => comparable.next_back(),
            Self::LastClean => comparable.rfind(|run| !run.dirty),
            Self::Commit(prefix) => {
                comparable.rfind(|run| run.commit.starts_with(prefix.as_str()))
            }
        }
    }
}

/// The history for one machine.
pub struct History {
    path: PathBuf,
}

impl History {
    /// `<root>/bench/history/<machine>.jsonl`.
    pub fn for_machine(root: &Path, machine: &Machine) -> Self {
        Self { path: root.join("bench/history").join(format!("{}.jsonl", machine.slug)) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every run, oldest first.
    ///
    /// A line that will not parse is skipped rather than fatal. The file is
    /// append-only and hand-editable, and one bad line — a partial write, a
    /// botched merge — should cost that line rather than the whole history.
    pub fn load(&self) -> Result<Vec<Run>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            // No file yet is an empty history, not a failure. It is what the
            // first run on a new machine looks like.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context(format!("reading {}", self.path.display())),
        };

        let mut runs = Vec::new();
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Run>(line) {
                Ok(run) if run.schema == SCHEMA => runs.push(run),
                Ok(run) => eprintln!(
                    "skipping {}:{}: written by schema {} and this is {SCHEMA}",
                    self.path.display(),
                    number + 1,
                    run.schema,
                ),
                Err(e) => eprintln!("skipping {}:{}: {e}", self.path.display(), number + 1),
            }
        }

        // By time rather than by file order, so a merge that interleaved two
        // branches' runs still reads chronologically.
        runs.sort_by_key(|run| run.recorded_at);
        Ok(runs)
    }

    /// Adds one run to the end.
    pub fn append(&self, run: &Run) -> Result<()> {
        use std::io::Write as _;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let line = serde_json::to_string(run).context("encoding the run")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;

        writeln!(file, "{line}").with_context(|| format!("writing {}", self.path.display()))
    }
}

/// What git says about the working tree.
struct Git {
    commit: String,
    branch: String,
    dirty: bool,
}

impl Git {
    fn read() -> Self {
        Self {
            commit: git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into()),
            branch: git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into()),
            // Any output at all from `status --porcelain` means something is
            // uncommitted. Nothing to parse: the presence of a line is the
            // answer.
            dirty: git(&["status", "--porcelain"]).is_some_and(|out| !out.trim().is_empty()),
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    match output.status.success() {
        true => Some(String::from_utf8(output.stdout).ok()?.trim().to_string()),
        false => None,
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{Summary, Unit};

    fn machine() -> Machine {
        Machine {
            slug: "test-machine".into(),
            cpu: "Test CPU".into(),
            model: "Test1,1".into(),
            cores: 8,
            memory: 0,
            os: "testOS".into(),
            arch: "aarch64".into(),
        }
    }

    fn run(commit: &str, at: i64, dirty: bool, corpus: usize, median: f64) -> Run {
        Run {
            schema: SCHEMA,
            recorded_at: at,
            commit: commit.into(),
            branch: "master".into(),
            dirty,
            corpus,
            suites: vec!["store".into()],
            machine: machine(),
            conditions: Conditions::default(),
            measurements: vec![Measurement {
                id: "store.list".into(),
                unit: Unit::Nanos,
                summary: Summary::of(vec![median], Unit::Nanos),
                batch: 1,
            }],
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spool-bench-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_machine_with_no_history_has_an_empty_one() {
        let history = History::for_machine(&scratch("empty"), &machine());
        assert!(history.load().unwrap().is_empty());
    }

    #[test]
    fn runs_append_and_read_back_in_order() {
        let root = scratch("append");
        let history = History::for_machine(&root, &machine());

        history.append(&run("aaa", 100, false, 1000, 50.0)).unwrap();
        history.append(&run("bbb", 200, false, 1000, 60.0)).unwrap();

        let loaded = history.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].commit, "aaa");
        assert_eq!(loaded[1].commit, "bbb");
        assert_eq!(loaded[1].measurement("store.list").unwrap().summary.median, 60.0);
        assert!(history.path().ends_with("test-machine.jsonl"));
    }

    #[test]
    fn out_of_order_lines_still_read_chronologically() {
        // What a merge of two branches' history files looks like.
        let root = scratch("order");
        let history = History::for_machine(&root, &machine());

        history.append(&run("late", 900, false, 1000, 50.0)).unwrap();
        history.append(&run("early", 100, false, 1000, 50.0)).unwrap();

        let loaded = history.load().unwrap();
        assert_eq!(loaded[0].commit, "early", "sorted by when, not by where in the file");
    }

    #[test]
    fn one_unreadable_line_costs_that_line_and_nothing_else() {
        let root = scratch("corrupt");
        let history = History::for_machine(&root, &machine());
        history.append(&run("good", 100, false, 1000, 50.0)).unwrap();

        // A half-written line, as a crash mid-append would leave.
        let mut text = std::fs::read_to_string(history.path()).unwrap();
        text.push_str("{\"schema\":1,\"recorded_at\":2\n");
        std::fs::write(history.path(), text).unwrap();

        history.append(&run("after", 300, false, 1000, 50.0)).unwrap();

        let loaded = history.load().unwrap();
        assert_eq!(loaded.len(), 2, "the good lines survive the bad one");
        assert_eq!(loaded[1].commit, "after");
    }

    #[test]
    fn a_run_from_another_schema_is_skipped_rather_than_misread() {
        let root = scratch("schema");
        let history = History::for_machine(&root, &machine());

        let mut old = run("ancient", 50, false, 1000, 50.0);
        old.schema = 0;
        history.append(&old).unwrap();
        history.append(&run("current", 100, false, 1000, 50.0)).unwrap();

        let loaded = history.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].commit, "current");
    }

    #[test]
    fn the_default_baseline_is_the_last_time_you_looked() {
        let runs = vec![
            run("aaa", 100, false, 1000, 50.0),
            run("bbb", 200, true, 1000, 55.0),
        ];
        assert_eq!(Baseline::Previous.find(&runs, 1000).unwrap().commit, "bbb");
    }

    #[test]
    fn a_clean_baseline_skips_the_dirty_experiments() {
        // The case it exists for: an afternoon of uncommitted attempts, and the
        // question is how far the whole afternoon moved things.
        let runs = vec![
            run("committed", 100, false, 1000, 50.0),
            run("try-one", 200, true, 1000, 80.0),
            run("try-two", 300, true, 1000, 90.0),
        ];

        assert_eq!(Baseline::Previous.find(&runs, 1000).unwrap().commit, "try-two");
        assert_eq!(Baseline::LastClean.find(&runs, 1000).unwrap().commit, "committed");
    }

    #[test]
    fn a_different_corpus_is_a_different_benchmark() {
        // Comparing a 10k run with a 100k one would report a tenfold
        // regression on every single line.
        let runs = vec![run("small", 100, false, 10_000, 5.0), run("big", 200, false, 100_000, 50.0)];

        assert_eq!(Baseline::Previous.find(&runs, 100_000).unwrap().commit, "big");
        assert_eq!(Baseline::Previous.find(&runs, 10_000).unwrap().commit, "small");
        assert!(Baseline::Previous.find(&runs, 250_000).is_none(), "nothing to compare with");
    }

    #[test]
    fn a_baseline_can_be_named_by_commit_prefix() {
        let runs = vec![
            run("9d2213f", 100, false, 1000, 50.0),
            run("78586f3", 200, false, 1000, 60.0),
        ];
        let found = Baseline::Commit("9d22".into()).find(&runs, 1000).unwrap();
        assert_eq!(found.commit, "9d2213f");
        assert!(Baseline::Commit("ffff".into()).find(&runs, 1000).is_none());
    }
}
