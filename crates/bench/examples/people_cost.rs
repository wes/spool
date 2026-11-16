//! Scratch: what the people index costs on a large mailbox — the one-off
//! backfill an existing store runs after upgrading, a batch of it (which is
//! how long a sync can wait behind one), and the two reads Clients makes.
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let count: usize = std::env::args().nth(1).and_then(|n| n.parse().ok()).unwrap_or(250_000);
    let path = std::env::args().nth(2).map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("spool-people-{count}.db")));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let store = spool_store::Store::open(&path)?;
    store.set_people_index(true)?;
    let t = Instant::now();
    spool_store::demo::populate(&store, count, 0x5EED)?;
    let ingest = t.elapsed();
    println!("ingest {count}: {ingest:?} ({:.0} msg/s), index written as it goes", count as f64 / ingest.as_secs_f64());

    store.reindex_people()?;
    let t = Instant::now();
    let mut batches = 0;
    let mut slowest = std::time::Duration::ZERO;
    loop {
        let b = Instant::now();
        let step = store.backfill_people(2_000)?;
        slowest = slowest.max(b.elapsed());
        batches += 1;
        if step.remaining == 0 {
            break;
        }
    }
    println!("backfill: {:?} in {batches} batches, slowest batch {slowest:?}", t.elapsed());

    let reader = store.people_reader()?.expect("a file-backed store has a reader");
    let t = Instant::now();
    let people = reader.people()?;
    println!("people(): {} correspondents in {:?}", people.len(), t.elapsed());
    let busiest = people.iter().max_by_key(|p| p.messages).expect("someone");
    let t = Instant::now();
    let history = reader.history(std::slice::from_ref(&busiest.addr), &[], 1_000)?;
    println!("history(busiest, {} msgs): {} entries in {:?}", busiest.messages, history.len(), t.elapsed());
    let t = Instant::now();
    let company = reader.history(&[], &["example.com".into()], 1_000)?;
    println!("history(@example.com): {} entries in {:?}", company.len(), t.elapsed());

    let rows: i64 = rusqlite::Connection::open(&path)?.query_row("SELECT COUNT(*) FROM message_people", [], |r| r.get(0))?;
    println!("message_people rows: {rows}");
    Ok(())
}
