//! Asks the server how much of a folder it considers unread.
//!
//! Read-only: a `SELECT` and a couple of `UID SEARCH`es, which is the server's
//! own answer to the question the sidebar is trying to answer. Exists to
//! settle disagreements between this client's count and another one's without
//! having to trust either.
//!
//!     cargo run -p spool-net --example unread_probe -- INBOX

fn main() -> anyhow::Result<()> {
    smol::block_on(async {
        let folder = std::env::args().nth(1).unwrap_or_else(|| "INBOX".into());
        let config = spool_net::Config::load(spool_net::Config::default_path())?;

        for account in &config.accounts {
            let credentials = account.credentials().await?;
            let mut session = spool_net::imap::connect_session(account, &credentials).await?;

            let status = session.select(&folder).await?;
            let unseen = session.uid_search("UNSEEN").await?;
            let recent = session.uid_search("SINCE 1-Sep-2026").await?;
            let recent_unseen = session.uid_search("UNSEEN SINCE 1-Sep-2026").await?;

            println!(
                "{}  {folder}: {} messages, {} unseen",
                account.email,
                status.exists,
                unseen.len(),
            );
            println!(
                "    since 1 Sep 2026: {} messages, {} unseen",
                recent.len(),
                recent_unseen.len(),
            );

            // How unread the *newest* slice is, which is the part any
            // client with a download limit actually holds. If the whole
            // folder is 67% unread but its newest 2,000 are 5%, then a
            // partial client's small number is honest and a big one is not.
            let exists = status.exists;
            for window in [500u32, 2_000, 5_000] {
                let first = exists.saturating_sub(window).max(1);
                let hits = session
                    .search(format!("UNSEEN {first}:{exists}"))
                    .await
                    .map(|h| h.len())
                    .unwrap_or(0);
                println!("    newest {window}: {hits} unseen");
            }

            session.logout().await.ok();
        }
        Ok(())
    })
}
