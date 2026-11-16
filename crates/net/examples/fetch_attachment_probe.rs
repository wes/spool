//! Fetches one attachment straight from the server, by message id.
//!
//! For checking the on-demand path end to end without the app:
//!
//!     cargo run -p spool-net --example fetch_attachment_probe -- <message-id> [index]
//!
//! Prints where the message lives and how many bytes came back. Writes
//! nothing anywhere.

use anyhow::{Context, Result};
use spool_net::Config;
use spool_core::MessageId;
use spool_store::Store;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let id: i64 = args.next().context("usage: <message-id> [index]")?.parse()?;
    let index: usize = args.next().unwrap_or_else(|| "0".into()).parse()?;

    let store = Store::open(spool_core::paths::data_dir().join("mail.db"))?;
    let origin = store
        .message_origin(MessageId(id))?
        .context("that message has no server to be fetched from")?;
    println!("message {id} is {} uid {} on {}", origin.mailbox, origin.uid, origin.email);

    let stored = store.attachment_bytes(MessageId(id), index)?;
    println!(
        "in the store: {}",
        match &stored {
            Some(bytes) => format!("{} bytes", bytes.len()),
            None => "nothing — this is the case worth testing".into(),
        }
    );

    let config = Config::load(Config::default_path())?;
    let account = config
        .accounts
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(&origin.email))
        .context("that account is not configured")?;

    println!("the server said this message is {} bytes", origin.size);

    // The same meter the reader draws its bar from, printed instead.
    let meter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let watched = meter.clone();
    let expected = origin.size;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = done.clone();
    std::thread::spawn(move || {
        while stop.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let read = watched.load(std::sync::atomic::Ordering::Relaxed);
            match expected {
                0 => println!("  read {read} bytes"),
                total => println!("  {:>3}%  ({read} of {total})", read * 100 / total as u64),
            }
        }
    });

    let started = std::time::Instant::now();
    let bytes = smol::block_on(spool_net::fetch_attachment(&account, &origin, index, meter))?;
    done.store(1, std::sync::atomic::Ordering::Relaxed);
    println!("fetched {} bytes in {:?}", bytes.len(), started.elapsed());
    println!(
        "looks like: {}",
        match bytes.get(..4) {
            Some(b"%PDF") => "a PDF",
            Some([0xff, 0xd8, ..]) => "a JPEG",
            Some([0x89, b'P', b'N', b'G']) => "a PNG",
            Some([0x50, 0x4b, ..]) => "a zip (docx/xlsx/…)",
            _ => "something else",
        }
    );
    Ok(())
}
