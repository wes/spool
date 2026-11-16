//! Fetches an image URL through the app's own HTTP client.
//!
//! Usage: cargo run -p spool-net --example fetch_image -- <url>

fn main() {
    let url = std::env::args().nth(1).expect("usage: fetch_image <url>");

    match smol::block_on(spool_net::http::get(&url, 8 * 1024 * 1024)) {
        Ok(bytes) => {
            let head: Vec<String> = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
            println!("ok: {} bytes, starts {}", bytes.len(), head.join(" "));
        }
        Err(e) => println!("FAILED: {e:#}"),
    }
}
