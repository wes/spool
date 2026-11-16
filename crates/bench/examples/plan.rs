//! What SQLite intends to do about the list queries.
//!
//! A benchmark says a query is slow. This says why — and the difference
//! matters, because the two plausible explanations for a slow `LIMIT 200` call
//! for opposite fixes. If the limit is being pushed into an index, the cost is
//! real work and the answer is to ask for less. If the whole table is being
//! sorted before the limit is applied, the cost is a missing index and the
//! query is doing five hundred times more work than it needs to.
//!
//! ```text
//! cargo run --release -p spool-bench --example plan
//! ```

use anyhow::Result;
use spool_bench::corpus::Corpus;

const CORPUS: usize = 50_000;

/// The queries the message list actually issues, in the shapes that matter.
const QUERIES: &[(&str, &str)] = &[
    (
        "everything, first page",
        "SELECT m.id FROM messages m ORDER BY m.date_ts DESC LIMIT 200",
    ),
    (
        "everything, all of it",
        "SELECT m.id FROM messages m ORDER BY m.date_ts DESC LIMIT 500000",
    ),
    (
        "one mailbox, first page",
        "SELECT m.id FROM messages m WHERE m.mailbox_id = 1 ORDER BY m.date_ts DESC LIMIT 200",
    ),
    (
        "one account, first page",
        "SELECT m.id FROM messages m WHERE m.account_id = 1 ORDER BY m.date_ts DESC LIMIT 200",
    ),
    (
        "everything, unread",
        "SELECT m.id FROM messages m WHERE (m.flags & 1) = 0 ORDER BY m.date_ts DESC LIMIT 200",
    ),
];

fn main() -> Result<()> {
    println!("building {CORPUS} messages\u{2026}\n");
    let corpus = Corpus::build(CORPUS)?;

    // Opened read-only and alongside, rather than through the store: the point
    // is to ask SQLite a question the store's API deliberately does not expose.
    let conn = rusqlite::Connection::open(corpus.path())?;

    for (label, sql) in QUERIES {
        println!("{label}");
        println!("  {sql}");

        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(3))?;
        for step in rows {
            println!("    {}", step?);
        }
        println!();
    }

    println!("`SCAN` with `USE TEMP B-TREE FOR ORDER BY` means the whole table is");
    println!("sorted before the limit applies \u{2014} the limit is saving nothing.");
    Ok(())
}
