//! How long counting every folder takes, which is what the sidebar needs
//! after every archive, delete and flag.
//!
//!     cargo run --release -p spool-store --example counts_cost

use anyhow::Result;
use spool_store::Store;

fn main() -> Result<()> {
    let store = Store::open(spool_core::paths::data_dir().join("mail.db"))?;

    for round in 1..=5 {
        let started = std::time::Instant::now();
        let mailboxes = store.mailboxes()?;
        let elapsed = started.elapsed();
        let total: u32 = mailboxes.iter().map(|m| m.total).sum();
        println!(
            "{round}: {:>8.1?}  ({} folders, {total} messages counted)",
            elapsed,
            mailboxes.len(),
        );
    }
    Ok(())
}
