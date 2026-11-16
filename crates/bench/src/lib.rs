//! Measuring Spool against itself, over time.
//!
//! This is the *internal* harness: headless, deterministic, and pointed at the
//! app's own code. It answers one question — "did this change make it slower?"
//! — and it answers it by keeping every previous answer in the repository.
//!
//! It is not the benchmark in `bench/`. That one measures Spool from the
//! outside, through synthetic clicks and screen capture, so that it can be
//! compared with mail clients whose source nobody has. Its numbers are in
//! milliseconds and include the window server; these are in microseconds and
//! include nothing but the code. Both are honest and they are not
//! interchangeable, which is why they are separate programs with separate
//! README files.
//!
//! The pieces:
//!
//! * [`corpus`] — the deterministic mailbox everything runs against.
//! * [`runner`] — turning code into samples.
//! * [`stats`] — turning samples into a number, and a difference into a verdict.
//! * [`machine`] — who measured it, and what state they were in.
//! * [`history`] — the record, one JSON object per run, per machine.
//! * [`report`] — the table, for one run against a baseline.
//! * [`page`] — the history as charts, for the trend across many.

pub mod corpus;
pub mod history;
pub mod machine;
pub mod page;
pub mod report;
pub mod runner;
pub mod stats;
pub mod suites;

use std::path::PathBuf;

/// The repository root, found from where this crate was compiled.
///
/// Resolved at compile time rather than from the working directory, so the
/// history lands in the same place whether the harness was started from the
/// repository root, from `crates/bench`, or by an editor with its own idea of
/// where it is.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}
