//! The store: ingest, listing, search, and the reads a message opening does.
//!
//! Where nearly all of the app's measurable cost actually is. Everything here
//! runs against one corpus built once, except ingest — which needs an empty
//! database by definition, and so builds its own each time.
//!
//! The listing benchmarks deliberately include the allocation of the result.
//! That is not an accident of how they are written: the app really does pull a
//! `Vec` of every envelope in scope into memory, and a version of these numbers
//! that measured only the SQLite side would be measuring a program nobody runs.

use anyhow::Result;
use spool_core::Flags;
use spool_store::{Query, Scope};

use crate::{
    corpus::{Corpus, NO_LIMIT, remove, scratch_path},
    runner::{Runner, black_box},
    stats::Unit,
};

/// How many times the ingest benchmark rebuilds the whole corpus.
///
/// Three, because each one costs seconds and a median of three is enough to
/// throw out a single interrupted run — which is all this number needs to be
/// robust against. More samples here would double the runtime of the suite to
/// tighten a figure that moves in whole percent when it moves at all.
const INGEST_SAMPLES: usize = 3;

/// Terms chosen for what they cost, with the shares measured rather than
/// guessed — `cargo run -p spool-bench --example terms` is what picked them,
/// and is what to re-run if `spool_store::demo` ever changes its vocabulary.
///
/// The first version of this table had it wrong in a way worth remembering: it
/// called `postmortem` rare and `deploy` common, and both are subject-line
/// words drawn from the same uniform list, so both matched about 4% and the
/// two benchmarks measured the same thing under different names.
///
/// Measured shares of a 20,000 message corpus:
///
/// | term             | share | what it exercises                        |
/// |------------------|-------|------------------------------------------|
/// | `the`            | 98.1% | the widest possible result set           |
/// | `week`           | 53.8% | half the corpus                          |
/// | `deploy`         |  4.0% | a selective term, where the index earns its keep |
/// | `security audit` |  4.1% | two words that have to match in order    |
///
/// Note what is *not* here: a term matching a handful of rows. The generated
/// corpus draws from a small vocabulary, so 4% is as selective as it gets, and
/// a benchmark claiming to measure a needle-in-a-haystack search would be
/// claiming something this corpus cannot support.
const SEARCHES: [(&str, &str); 4] = [
    ("wide", "the"),
    ("common", "week"),
    ("selective", "deploy"),
    ("phrase", "security audit"),
];

pub fn run(runner: &mut Runner, corpus: &Corpus) -> Result<()> {
    let store = &corpus.store;
    let inbox = corpus.inbox()?;

    // --- what the list does ------------------------------------------------
    runner.bench("store.list.all", || {
        black_box(store.envelopes(Scope::Everything, &Query::All, NO_LIMIT).unwrap());
    });
    runner.bench("store.list.unread", || {
        black_box(store.envelopes(Scope::Everything, &Query::Unread, NO_LIMIT).unwrap());
    });
    runner.bench("store.list.flagged", || {
        black_box(store.envelopes(Scope::Everything, &Query::Flagged, NO_LIMIT).unwrap());
    });
    runner.bench("store.list.inbox", || {
        black_box(store.envelopes(Scope::Mailbox(inbox), &Query::All, NO_LIMIT).unwrap());
    });

    // Conversations fold in Rust rather than in SQL, so this is the listing
    // cost plus the threading cost, and the difference between the two rows is
    // what grouping actually charges.
    runner.bench("store.conversations", || {
        black_box(store.conversations(Scope::Everything, &Query::All, NO_LIMIT).unwrap());
    });

    // The first page, which is what a window actually paints. Kept beside the
    // unbounded versions because it is the number a person waits for, where
    // the others are what the app pulls into memory behind them.
    runner.bench("store.list.page", || {
        black_box(store.envelopes(Scope::Everything, &Query::All, 200).unwrap());
    });

    // --- search ------------------------------------------------------------
    for (kind, term) in SEARCHES {
        runner.bench(format!("store.search.{kind}"), || {
            black_box(
                store
                    .envelopes(Scope::Everything, &Query::Text(term.into()), NO_LIMIT)
                    .unwrap(),
            );
        });
    }

    // --- opening a message -------------------------------------------------
    let sample = corpus.envelopes(64)?;
    let first = sample.first().map(|e| e.id);

    if let Some(id) = first {
        runner.bench("store.body", || {
            black_box(store.body(id).unwrap());
        });
    }

    if let Some(thread) = sample.first().map(|e| e.thread_id) {
        runner.bench("store.thread", || {
            black_box(store.thread(thread).unwrap());
        });
    }

    // --- the client book ---------------------------------------------------
    // What opening Clients waits for. `people` is everybody in the index,
    // counted up — one pass over every row of it, which is the cost that
    // grows with the mailbox — and `history` is one person's mail, which is a
    // seek into the index and should not grow with it at all. The busiest
    // correspondent is the worst case for the second.
    // With Clients on, as it has to be for any of this to exist: the index is
    // built for the corpus first, and switched off again after, so the
    // writing benchmarks below measure the store as most people run it.
    store.set_people_index(true)?;
    while store.backfill_people(2_000)?.remaining > 0 {}
    let people = store.people()?;
    runner.bench("store.people", || {
        black_box(store.people().unwrap());
    });
    if let Some(busiest) = people.iter().max_by_key(|p| p.messages) {
        let addrs = vec![busiest.addr.clone()];
        runner.bench("store.history", || {
            black_box(store.history(&addrs, &[], 1_000).unwrap());
        });
    }
    runner.record("store.rows.people", Unit::Count, people.len() as f64);

    // The one-off pass that indexes mail stored before the index existed,
    // which every existing mailbox runs once after upgrading. In messages a
    // second, so it reads as a rate against ingest's. The index is emptied
    // before each sample and is whole again after it, so the corpus is left
    // as it was found.
    let messages = store.message_count()? as usize;
    runner.bench_throughput(
        "store.backfill",
        messages,
        INGEST_SAMPLES,
        || store.reindex_people().expect("emptying the people index"),
        |_| while store.backfill_people(2_000).expect("indexing people").remaining > 0 {},
    );
    store.set_people_index(false)?;

    // --- writing -----------------------------------------------------------
    // The corpus is restored afterwards rather than left to an even number of
    // toggles. The iteration count is chosen at run time from how fast the
    // machine is, so "it ends where it started" would be true on some machines
    // and not others — and a corpus that quietly differs is one every
    // benchmark after this measures differently.
    if let Some(envelope) = sample.first() {
        let id = envelope.id;
        let original = envelope.flags;

        let mut flags = original;
        runner.bench("store.set_flags", || {
            flags = flags.toggled(Flags::FLAGGED);
            store.set_flags(id, flags).unwrap();
        });

        store.set_flags(id, original)?;
    }

    // --- start-up ----------------------------------------------------------
    // What a launch waits for. The app opens the store on the main thread
    // before there is an `NSApplication`, let alone a window, so every
    // millisecond here is a millisecond of dock icon bouncing.
    //
    // It is in the suite because this is the number that regressed silently
    // once already: `open` used to seed the threader, which reads every row in
    // `messages`, and nothing measured it. On a 220,000-message mailbox that
    // was two seconds on a cold page cache and sixty milliseconds on a warm
    // one — a start-up that was either instant or not depending on what else
    // the machine had been doing, with nothing in the harness to say so.
    //
    // Warm by construction, since the corpus was just written. That makes this
    // a floor rather than a worst case: it cannot reproduce a cold cache, but
    // anything that puts a scan back into `open` shows up here immediately,
    // because a scan costs far more than the open it is hiding in even warm.
    //
    // It also matters that the corpus is still open while this runs. Each
    // iteration drops the store it made, and dropping the *last* connection to
    // a WAL database checkpoints it — so a version of this benchmark that let
    // the file go idle would be timing a checkpoint and calling it an open.
    let path = corpus.path().to_path_buf();
    runner.bench("store.open", || {
        black_box(spool_store::Store::open(&path).unwrap());
    });

    // --- what the corpus actually is ---------------------------------------
    // Counts, not timings. They describe the workload, so a run where search
    // got faster because it stopped matching anything is visible as exactly
    // that.
    runner.record("store.size.disk", Unit::Bytes, corpus.size_on_disk()? as f64);
    runner.record(
        "store.rows.all",
        Unit::Count,
        store.envelopes(Scope::Everything, &Query::All, NO_LIMIT)?.len() as f64,
    );
    for (kind, term) in SEARCHES {
        let matched = store
            .envelopes(Scope::Everything, &Query::Text(term.into()), NO_LIMIT)?
            .len();
        runner.record(format!("store.rows.{kind}"), Unit::Count, matched as f64);
    }

    Ok(())
}

/// Ingest, which needs an empty database and so cannot share the corpus.
///
/// Separate from [`run`] because it is by far the most expensive thing in the
/// suite — it builds the entire corpus from scratch, three times — and is
/// therefore the one benchmark worth being able to skip.
pub fn ingest(runner: &mut Runner, count: usize) -> Result<()> {
    // One path, taken once and reused across the samples. Asking for a fresh
    // one per sample would leave every earlier sample's database behind, and
    // asking for one again at the end would remove a file this never wrote.
    let path = scratch_path(count);

    runner.bench_throughput(
        "store.ingest",
        count,
        INGEST_SAMPLES,
        || {
            // Setup is outside the clock: a fresh file, an open database, and
            // the schema migration. None of that is ingest.
            remove(&path);
            spool_store::Store::open(&path).expect("opening a fresh benchmark store")
        },
        |store| {
            spool_store::demo::populate(store, count, crate::corpus::SEED)
                .expect("generating the corpus");
        },
    );

    remove(&path);
    Ok(())
}

/// Every benchmark id this suite can produce, for `--list`.
pub fn ids() -> Vec<String> {
    let mut ids = vec![
        "store.ingest".to_string(),
        "store.list.all".into(),
        "store.list.unread".into(),
        "store.list.flagged".into(),
        "store.list.inbox".into(),
        "store.conversations".into(),
        "store.list.page".into(),
    ];
    ids.extend(SEARCHES.iter().map(|(kind, _)| format!("store.search.{kind}")));
    ids.extend(["store.body".into(), "store.thread".into()]);
    ids.extend(["store.people".into(), "store.history".into(), "store.rows.people".into(), "store.backfill".into()]);
    ids.push("store.set_flags".into());
    ids.push("store.open".into());
    ids.extend(["store.size.disk".into(), "store.rows.all".into()]);
    ids.extend(SEARCHES.iter().map(|(kind, _)| format!("store.rows.{kind}")));
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Sampling;

    #[test]
    fn the_suite_measures_everything_it_claims_to() {
        // `ids()` is what `--list` prints and what a person greps for. A
        // benchmark that exists and is not listed is one nobody finds; a
        // listed one that does not exist is worse.
        let corpus = Corpus::build(400).unwrap();
        let mut runner = Runner::new(Sampling::quick());
        run(&mut runner, &corpus).unwrap();

        let produced: Vec<String> =
            runner.into_measurements().into_iter().map(|m| m.id).collect();

        for id in ids() {
            // Ingest is the one `run` does not produce, having its own entry
            // point.
            if id == "store.ingest" {
                continue;
            }
            assert!(produced.contains(&id), "`{id}` is listed but was never measured");
        }
    }

    #[test]
    fn ingest_is_reported_as_messages_per_second() {
        let mut runner = Runner::new(Sampling::quick());
        ingest(&mut runner, 200).unwrap();

        let measured = runner.into_measurements();
        assert_eq!(measured[0].id, "store.ingest");
        assert_eq!(measured[0].unit, Unit::PerSecond);
        assert!(measured[0].summary.median > 0.0);
    }

    #[test]
    fn the_corpus_is_left_exactly_as_it_was_found() {
        // `store.set_flags` writes. If it left the corpus changed, every
        // benchmark after it would be measuring a different mailbox — and the
        // run after that would be comparing against one.
        let corpus = Corpus::build(400).unwrap();
        let before = corpus
            .store
            .envelopes(Scope::Everything, &Query::Flagged, NO_LIMIT)
            .unwrap()
            .len();

        let mut runner = Runner::new(Sampling::quick());
        run(&mut runner, &corpus).unwrap();

        let after = corpus
            .store
            .envelopes(Scope::Everything, &Query::Flagged, NO_LIMIT)
            .unwrap()
            .len();
        assert_eq!(before, after, "the flag benchmark has to put the corpus back");
    }

    #[test]
    fn searches_match_something_worth_timing() {
        // A search term that matches nothing is a fast benchmark measuring an
        // empty result set. The counts are recorded for exactly this reason,
        // and this asserts the recording is not of zero.
        let corpus = Corpus::build(2_000).unwrap();
        let mut runner = Runner::new(Sampling::quick());
        run(&mut runner, &corpus).unwrap();

        let measured = runner.into_measurements();
        let common = measured.iter().find(|m| m.id == "store.rows.common").unwrap();
        assert!(common.summary.median > 0.0, "`deploy` should match in a 2,000 message corpus");
    }

    #[test]
    fn a_mailbox_scope_is_narrower_than_everything() {
        // Not a timing claim — a sanity check that the scoped benchmark is
        // actually scoped, rather than quietly measuring the same query twice.
        let corpus = Corpus::build(2_000).unwrap();
        let inbox = corpus.inbox().unwrap();

        let all = corpus.store.envelopes(Scope::Everything, &Query::All, NO_LIMIT).unwrap().len();
        let in_inbox =
            corpus.store.envelopes(Scope::Mailbox(inbox), &Query::All, NO_LIMIT).unwrap().len();

        assert!(in_inbox > 0 && in_inbox < all, "inbox {in_inbox} of {all}");
    }
}
