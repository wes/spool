//! Which search terms the benchmarks should use.
//!
//! The search benchmarks claim to measure a common term, a rare one and a
//! phrase. That claim is only true if the terms actually have those
//! frequencies in the generated corpus — and the first version of this suite
//! got it wrong, labelling as "rare" a word that matched more rows than the
//! "common" one, because both came from the same uniformly-sampled list of
//! subject lines.
//!
//! So the terms are chosen by measurement rather than by intuition. Run this
//! after changing `spool_store::demo`, which is the only thing that can
//! invalidate the choice:
//!
//! ```text
//! cargo run --release -p spool-bench --example terms
//! ```

use anyhow::Result;
use spool_bench::corpus::{Corpus, NO_LIMIT};
use spool_store::{Query, Scope};

/// Every word worth considering, from each of the places the generator draws
/// text: subject heads, subject tails, and body sentences.
const CANDIDATES: &[&str] = &[
    // From the sentences, which every message draws several of.
    "the", "this", "week", "change", "look", "version", "timeline", "morning",
    // From the subject heads, each about one in twenty-five.
    "deploy", "postmortem", "invoice", "compliance", "offsite", "renewal", "migration",
    // Deliberately unusual words, each in exactly one sentence.
    "ballpark", "vendor", "staging", "inline",
    // Phrases, which have to match two words in order.
    "security audit", "staging environment", "edge case", "action items",
];

const CORPUS: usize = 20_000;

fn main() -> Result<()> {
    println!("building {CORPUS} messages\u{2026}");
    let corpus = Corpus::build(CORPUS)?;

    let mut counted: Vec<(usize, &str)> = CANDIDATES
        .iter()
        .map(|term| {
            let matched = corpus
                .store
                .envelopes(Scope::Everything, &Query::Text((*term).into()), NO_LIMIT)
                .map(|rows| rows.len())
                .unwrap_or(0);
            (matched, *term)
        })
        .collect();

    counted.sort_by_key(|(matched, _)| std::cmp::Reverse(*matched));

    println!("\n{:<22} {:>8} {:>8}", "term", "rows", "share");
    for (matched, term) in &counted {
        println!(
            "{term:<22} {matched:>8} {:>7.1}%",
            *matched as f64 / CORPUS as f64 * 100.0
        );
    }

    println!(
        "\nPick `common` from the top, `rare` from the bottom, and a `phrase` that \
         matches enough rows to be doing work."
    );
    Ok(())
}
