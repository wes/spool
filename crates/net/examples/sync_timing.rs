//! What each kind of sync costs, against the real accounts.
//!
//! Checking for new mail opens the inboxes; a full sync lists the account and
//! walks every folder in it. The difference is the whole reason there are two,
//! and this is how to see it rather than assume it.
//!
//! Writes into whatever store it is pointed at — new mail is downloaded, the
//! same as a sync in the app — so point it at a copy, not at the mail store
//! the app has open.
//!
//!     cargo run -p spool-net --example sync_timing -- /tmp/copy.db
//!     cargo run -p spool-net --example sync_timing -- /tmp/copy.db inboxes

use std::time::Instant;

use spool_net::SyncMode;

fn main() -> anyhow::Result<()> {
    smol::block_on(async {
        let mut args = std::env::args().skip(1);
        let path = args.next().unwrap_or_else(|| "/tmp/spool-sync-timing.db".into());
        let modes: Vec<SyncMode> = match args.next().as_deref() {
            Some("inboxes") => vec![SyncMode::Inboxes],
            Some("everything") => vec![SyncMode::Everything],
            // Both, inboxes first: the cheap one should not be paying for the
            // expensive one's warm connection.
            _ => vec![SyncMode::Inboxes, SyncMode::Everything],
        };

        let store = spool_store::Store::open(&path)?;
        let config = spool_net::Config::load(spool_net::Config::default_path())?;

        for mode in modes {
            println!("\n{mode:?}");
            for account in &config.accounts {
                let started = Instant::now();
                let folders = std::cell::Cell::new(0usize);

                let report = spool_net::sync_account(
                    store.clone(),
                    account.clone(),
                    mode,
                    |progress| {
                        if let spool_net::Progress::Folder { .. } = progress {
                            folders.set(folders.get() + 1);
                        }
                    },
                )
                .await;

                let elapsed = started.elapsed();
                match report {
                    Ok(report) => println!(
                        "  {:<24} {:>6.1}s  {} folders  {}",
                        account.email,
                        elapsed.as_secs_f32(),
                        folders.get(),
                        report.summary(),
                    ),
                    Err(e) => println!(
                        "  {:<24} {:>6.1}s  failed: {e:#}",
                        account.email,
                        elapsed.as_secs_f32(),
                    ),
                }
            }
        }

        Ok(())
    })
}
