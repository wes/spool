//! Scratch: what a launch pays the store.
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let count: usize = std::env::args().nth(1).unwrap_or("220000".into()).parse()?;
    let path = std::env::temp_dir().join("spool-startup-probe.db");
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }

    let build = Instant::now();
    let store = spool_store::Store::open(&path)?;
    spool_store::demo::populate(&store, count, 0x5EED)?;
    println!("corpus of {count} built in {:?}", build.elapsed());
    drop(store);

    for round in 1..=3 {
        let t = Instant::now();
        let store = spool_store::Store::open(&path)?;
        println!("  open #{round}: {:?}", t.elapsed());
        drop(store);
    }

    println!("db {} bytes", std::fs::metadata(&path)?.len());
    Ok(())
}
