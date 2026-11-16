//! Pulling contacts from a source: an endpoint that lists your signups.
//!
//! A source is a URL that answers with JSON or CSV. That is the whole
//! contract, chosen because it is the one every stack can meet in an
//! afternoon — an admin route that dumps the users table is a source, and so
//! is a spreadsheet published as CSV. Nothing about it is specific to any one
//! product, which is what lets a product's signups and a newsletter tool's
//! subscribers arrive the same way.
//!
//! Pulls are idempotent by design. Contacts are matched to clients by the
//! source's own id and then by address, so pulling the whole list every hour
//! is correct, merely wasteful. A source that can answer "who is new since"
//! says so by putting `{since}` in its URL, and is asked only for that.

use anyhow::{Context as _, Result, bail};
use spool_core::contacts::{self, Contact};
use spool_net::secrets::{self, Slot};
use spool_store::book::{Source, SourceFormat};

/// The most a pull will read. A list of signups is text; a hundred thousand
/// rows of it is a few megabytes, and this is ten times that.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// How long a pull waits before giving up on a server that has stopped
/// talking.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The keychain name a source's token is stored under.
///
/// By id rather than by name, so renaming a source keeps its token.
pub fn token_key(source: i64) -> String {
    format!("source-{source}")
}

/// Stores a token, or forgets it when empty. Blocks on the keychain; call it
/// off the UI thread.
pub fn store_token(source: i64, token: &str) -> Result<()> {
    match token.trim().is_empty() {
        true => secrets::delete(Slot::ClientSource, &token_key(source)),
        false => secrets::store(Slot::ClientSource, &token_key(source), token.trim()),
    }
}

/// Whether a token is stored. Blocks on the keychain; call it off the UI
/// thread.
pub fn has_token(source: i64) -> bool {
    secrets::load(Slot::ClientSource, &token_key(source)).ok().flatten().is_some()
}

/// The URL to ask, with `{since}` and `{since_iso}` filled in.
///
/// A source that has never worked is asked from the beginning of time, which
/// is the epoch for the number and its date for the ISO form.
pub fn resolve_url(url: &str, last_success: Option<i64>) -> String {
    let since = last_success.unwrap_or(0);
    url.replace("{since}", &since.to_string()).replace("{since_iso}", &iso8601(since))
}

fn iso8601(ts: i64) -> String {
    let (y, mo, d, h, mi) = spool_core::civil_from_unix(ts);
    let s = ts.rem_euclid(60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The header a token is sent in.
///
/// A token that already says what it is — `Bearer …`, `Basic …`, `Token …` —
/// is sent as written; a bare one is sent as a bearer token, which is what
/// nearly every API that takes a key in a header expects.
fn authorization(token: &str) -> String {
    let token = token.trim();
    let scheme = token.split_whitespace().next().unwrap_or("");
    match ["bearer", "basic", "token", "apikey"].contains(&scheme.to_ascii_lowercase().as_str()) && token.contains(' ') {
        true => token.to_string(),
        false => format!("Bearer {token}"),
    }
}

/// Fetches a source and reads what it sends into contacts.
///
/// Runs its network on a background thread and blocks there, the way the
/// extension host's `fetch` does: the request is a `smol` future, and the
/// timeout is ours to enforce.
pub async fn pull(source: &Source, _started: i64) -> Result<Vec<Contact>> {
    let url = resolve_url(&source.url, source.last_success);
    let id = source.id;
    let format = source.format;

    let reply = smol::unblock(move || {
        // In here with the request: reading the keychain runs `security(1)`,
        // a process of its own, and the caller is on the UI thread.
        let token = secrets::load(Slot::ClientSource, &token_key(id)).ok().flatten();
        let mut headers = vec![(
            "Accept".to_string(),
            match format {
                SourceFormat::Json => "application/json".to_string(),
                SourceFormat::Csv => "text/csv".to_string(),
            },
        )];
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), authorization(&token)));
        }
        smol::block_on(async {
            smol::future::or(
                async { Some(spool_net::http::request("GET", &url, &headers, None, MAX_BYTES).await) },
                async {
                    smol::Timer::after(TIMEOUT).await;
                    None
                },
            )
            .await
        })
    })
    .await;

    let reply = match reply {
        Some(reply) => reply.context("could not reach the source")?,
        None => bail!("the source did not answer within a minute"),
    };
    match reply.status {
        200..=299 => {}
        401 | 403 => bail!("the source refused the token (HTTP {})", reply.status),
        301 | 302 | 303 | 307 | 308 => bail!("the source redirects (HTTP {}); use the address it redirects to", reply.status),
        status => bail!("the source answered HTTP {status}"),
    }

    let text = String::from_utf8_lossy(&reply.body);
    read(&text, format)
}

/// Reads a source's answer.
pub fn read(text: &str, format: SourceFormat) -> Result<Vec<Contact>> {
    let table = match format {
        SourceFormat::Json => contacts::parse_json(text).map_err(anyhow::Error::msg)?,
        SourceFormat::Csv => contacts::parse_csv(text),
    };
    let columns = contacts::guess(&table);
    if !columns.contains(&contacts::Column::Email) {
        bail!("nothing in the answer looks like an email address");
    }
    Ok(contacts::contacts(&table, &columns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_the_last_pull_that_worked() {
        let url = "https://camp.example/admin/signups.json?after={since}&iso={since_iso}";
        assert_eq!(
            resolve_url(url, Some(1_760_000_000)),
            "https://camp.example/admin/signups.json?after=1760000000&iso=2025-10-09T08:53:20Z"
        );
        assert_eq!(
            resolve_url(url, None),
            "https://camp.example/admin/signups.json?after=0&iso=1970-01-01T00:00:00Z",
            "never pulled is everyone"
        );
    }

    #[test]
    fn a_bare_token_is_a_bearer_and_a_scheme_is_kept() {
        assert_eq!(authorization("abc123"), "Bearer abc123");
        assert_eq!(authorization("Basic dXNlcjpwYXNz"), "Basic dXNlcjpwYXNz");
        assert_eq!(authorization("  Bearer xyz "), "Bearer xyz");
    }

    #[test]
    fn an_answer_with_no_addresses_is_an_error_not_an_empty_pull() {
        assert!(read("[{\"id\": 1, \"name\": \"Ana\"}]", SourceFormat::Json).is_err());
        let contacts = read("[{\"id\": 1, \"email\": \"ana@acme.com\"}]", SourceFormat::Json).unwrap();
        assert_eq!(contacts[0].external_id, "1");
        let csv = read("email,name\nbo@acme.com,Bo\n", SourceFormat::Csv).unwrap();
        assert_eq!(csv[0].name, "Bo");
    }
}
