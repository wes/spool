//! The mailbox every benchmark runs against.
//!
//! Built from `spool_store::demo`, at a fixed seed, so the corpus is a function
//! of its size and nothing else. That is what makes two runs on two days
//! comparable at all: the same messages, the same threads, the same
//! distribution of flags and HTML shapes, the same words for search to find.
//!
//! It lives in the temporary directory and is deleted afterwards. Not in the
//! repository, because it is hundreds of megabytes; not in the data directory,
//! because a benchmark has no business anywhere near somebody's real mail.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use spool_core::{Body, Envelope, MailboxId};
use spool_store::{Query, Scope, Store};

/// The seed the app's own `--demo` uses. Shared on purpose: the corpus you
/// benchmark is the corpus you can open and look at.
pub const SEED: u64 = 0x5EED;

/// More than any query here asks for, so a limit never truncates a result and
/// silently turns a benchmark into a smaller one.
pub const NO_LIMIT: usize = 500_000;

pub struct Corpus {
    pub store: Store,
    pub count: usize,
    path: PathBuf,
}

impl Corpus {
    /// Builds a fresh corpus of `count` messages.
    pub fn build(count: usize) -> Result<Self> {
        let path = scratch_path(count);
        remove(&path);

        let store = Store::open(&path)
            .with_context(|| format!("opening the benchmark store at {}", path.display()))?;
        spool_store::demo::populate(&store, count, SEED).context("generating the corpus")?;

        Ok(Self { store, count, path })
    }

    /// The inbox, which is where most of the corpus is.
    pub fn inbox(&self) -> Result<MailboxId> {
        let mailboxes = self.store.mailboxes()?;
        mailboxes
            .iter()
            .find(|m| m.role == spool_core::MailboxRole::Inbox)
            .map(|m| m.id)
            .context("the generated corpus has no inbox")
    }

    /// A handful of envelopes, for benchmarks that need a message to act on.
    pub fn envelopes(&self, wanted: usize) -> Result<Vec<Envelope>> {
        self.store.envelopes(Scope::Everything, &Query::All, wanted)
    }

    /// Bodies that actually carry HTML.
    ///
    /// A third of the corpus is `multipart/related` with an HTML alternative,
    /// so this walks until it has enough rather than taking the first `wanted`
    /// — benchmarking the HTML renderer against a plain-text body would
    /// measure the early return.
    pub fn html_bodies(&self, wanted: usize) -> Result<Vec<Body>> {
        let mut found = Vec::with_capacity(wanted);

        for envelope in self.store.envelopes(Scope::Everything, &Query::All, NO_LIMIT)? {
            if found.len() >= wanted {
                break;
            }
            if let Some(body) = self.store.body(envelope.id)?
                && spool_core::html::is_document(&body.html)
            {
                found.push(body);
            }
        }

        Ok(found)
    }

    /// Where the database file is.
    ///
    /// For tools that need to ask SQLite something the store's API does not
    /// expose — `--example plan` and nothing else.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn size_on_disk(&self) -> Result<u64> {
        self.store.size_on_disk()
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        remove(&self.path);
    }
}

/// Where a store goes. A different place every time it is asked.
///
/// The process id keeps two `spool-bench` runs apart and the count says what
/// the file holds, but neither is enough on its own: one process builds several
/// corpora of the same size, and deriving the path from the size alone had them
/// sharing a file. The ingest benchmark would then delete and rebuild the very
/// database the read benchmarks were holding open, and delete it again on the
/// way out — which on macOS silently kept working, because an unlinked file
/// stays alive for whoever has it open, right up until it did not.
///
/// So: a serial number, and every caller gets its own.
pub fn scratch_path(count: usize) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("spool-bench-{}-{count}-{serial}.db", std::process::id()))
}

/// Removes a store and the two files SQLite keeps beside it.
pub fn remove(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_corpus_builds_and_is_the_size_it_was_asked_for() {
        let corpus = Corpus::build(500).expect("it builds");
        assert_eq!(corpus.store.message_count().unwrap(), 500);
        assert!(corpus.inbox().is_ok(), "the demo corpus always has an inbox");
        assert!(corpus.size_on_disk().unwrap() > 0);
    }

    #[test]
    fn the_same_size_always_produces_the_same_corpus() {
        // The property every comparison over time depends on. If this stops
        // holding, every historical number becomes incomparable at once.
        let first = Corpus::build(300).unwrap();
        let subjects_first: Vec<String> =
            first.envelopes(20).unwrap().into_iter().map(|e| e.subject).collect();
        drop(first);

        let second = Corpus::build(300).unwrap();
        let subjects_second: Vec<String> =
            second.envelopes(20).unwrap().into_iter().map(|e| e.subject).collect();

        assert_eq!(subjects_first, subjects_second);
    }

    #[test]
    fn html_bodies_are_found_rather_than_hoped_for() {
        // Benchmarking the HTML renderer against a plain-text body measures
        // the early return, which is a fast number about nothing.
        let corpus = Corpus::build(600).unwrap();
        let bodies = corpus.html_bodies(5).unwrap();

        assert_eq!(bodies.len(), 5, "a third of the corpus is rich");
        assert!(bodies.iter().all(|b| spool_core::html::is_document(&b.html)));
    }

    #[test]
    fn two_corpora_of_one_size_do_not_share_a_database() {
        // The bug this guards: the ingest benchmark builds its own corpus at
        // the same size as the one the read benchmarks are holding open. With
        // the path derived from the size alone, ingest was deleting and
        // rebuilding the database underneath them — and then deleting it again
        // on the way out.
        let first = Corpus::build(200).unwrap();
        let second = Corpus::build(200).unwrap();
        assert_ne!(first.path(), second.path());

        drop(second);
        // The first is still there and still readable, which is the property
        // that was actually broken.
        assert!(first.path().exists());
        assert_eq!(first.store.message_count().unwrap(), 200);
    }

    #[test]
    fn every_path_asked_for_is_a_different_one() {
        let paths: Vec<_> = (0..5).map(|_| scratch_path(1_000)).collect();
        let unique: std::collections::BTreeSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "{paths:#?}");
    }

    #[test]
    fn the_database_is_gone_once_the_corpus_is() {
        let path = {
            let corpus = Corpus::build(100).unwrap();
            corpus.path.clone()
        };
        assert!(!path.exists(), "a benchmark should not leave a database behind");
    }
}
