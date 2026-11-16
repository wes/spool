//! Just enough HTTPS to talk to a token endpoint.
//!
//! The OAuth flow needs exactly one thing from an HTTP client: POST a form to
//! a known host and read the JSON back. A general-purpose client would bring a
//! second TLS stack and a second async runtime into a crate that already has
//! one of each, so this does the small specific thing instead.
//!
//! It is deliberately not a web client: no keep-alive, no cookies, no cache.
//! It does decode chunked transfer, because asking for `Connection: close`
//! does not stop a server from chunking — Google's token endpoint sends
//! `Transfer-Encoding: chunked` regardless, and reading its body as-is leaves
//! the hex chunk length sitting in front of the JSON.
//!
//! It also fetches images, for the messages whose reader has asked to load
//! them. That path follows redirects, since image URLs in mail almost always
//! go through at least one.

use anyhow::{Context, Result, bail};
use futures::AsyncReadExt;
use futures::io::AsyncWriteExt;

use crate::config::Security;
use crate::stream::Stream;

/// The response body, with the status left for the caller to judge.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// POSTs `application/x-www-form-urlencoded` over TLS and returns the body.
///
/// A non-2xx status is returned rather than raised: OAuth endpoints put the
/// reason for a refusal in the body of a 400, and that reason is the whole
/// value of the call.
pub async fn post_form(host: &str, path: &str, fields: &[(&str, &str)]) -> Result<Response> {
    let body = encode_form(fields);

    let tcp = smol::net::TcpStream::connect((host, 443))
        .await
        .with_context(|| format!("connecting to {host}"))?;
    let mut tls = async_native_tls::TlsConnector::new()
        .connect(host, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: spool/{version}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {length}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        version = env!("CARGO_PKG_VERSION"),
        length = body.len(),
    );

    tls.write_all(request.as_bytes())
        .await
        .with_context(|| format!("sending the request to {host}"))?;
    tls.flush().await.ok();

    let mut raw = Vec::new();
    tls.read_to_end(&mut raw)
        .await
        .with_context(|| format!("reading the response from {host}"))?;

    parse_response(&raw)
}

/// POSTs a form to a full `https:` URL and returns the status.
///
/// For RFC 8058's one-click unsubscribe, which is why it is this shape: HTTPS
/// only, no redirects followed, and nothing sent but the form — no cookies, no
/// referer, no credentials, even ones written into the URL. The body is read
/// only as far as the cap and then dropped; the status is the whole answer.
pub async fn post_form_to(url: &str, fields: &[(&str, &str)]) -> Result<u16> {
    let url = Url::parse(url)?;
    if !url.secure {
        bail!("refusing to send this over plain HTTP");
    }
    let body = encode_form(fields);

    let mut stream = Stream::connect(&url.host, url.port, Security::Tls).await?;
    let request = format!(
        "POST {target} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: spool/{version}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {length}\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        target = url.target,
        host = url.host,
        version = env!("CARGO_PKG_VERSION"),
        length = body.len(),
    );

    stream
        .write_all(request.as_bytes())
        .await
        .with_context(|| format!("sending the request to {}", url.host))?;
    stream.flush().await.ok();

    let mut raw = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading from {}", url.host))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        // The status line is all that is wanted; a page of thanks can be cut
        // short rather than read to the end.
        if raw.len() > MAX_HEADER_BYTES || split_head(&raw).is_some() {
            break;
        }
    }

    let head = split_head(&raw).map_or(&raw[..], |(head, _)| head);
    let head = String::from_utf8_lossy(head);
    let status_line = head.lines().next().unwrap_or_default();
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("unreadable status line: {status_line:?}"))
}

/// A parsed absolute URL, which is all this client needs to dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path plus query, as it goes on the request line.
    pub target: String,
    pub secure: bool,
}

impl Url {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .context("a URL needs a scheme")?;

        let secure = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            other => bail!("unsupported URL scheme: {other}"),
        };

        // Authority runs to the first `/`, `?` or `#`.
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);

        // Credentials in a URL are not something to send anywhere.
        let authority = authority.rsplit('@').next().unwrap_or(authority);

        let (host, port) = match authority.rsplit_once(':') {
            // Not a port if it is not a number — an IPv6 literal, say.
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h, p.parse().unwrap_or(if secure { 443 } else { 80 }))
            }
            _ => (authority, if secure { 443 } else { 80 }),
        };

        let host = host.trim_matches(['[', ']']).to_string();
        if host.is_empty() {
            bail!("a URL needs a host");
        }

        let target = match path {
            "" => "/".to_string(),
            // The fragment is a client-side concern and is never sent.
            path => path.split('#').next().unwrap_or("/").to_string(),
        };

        Ok(Self { host, port, target, secure })
    }
}

/// How many redirects to follow before giving up. Image URLs in mail redirect
/// through trackers and CDNs routinely; a loop is the thing to stop.
const MAX_REDIRECTS: usize = 5;

/// Fetches a URL and returns its body, up to `max_bytes`.
///
/// Used for one thing: an image a message asked for, after the reader chose to
/// load it. It sends nothing about the reader beyond what a request needs —
/// no cookies, no referer — because the request itself is already telling the
/// sender that their mail was opened.
pub async fn get(url: &str, max_bytes: usize) -> Result<Vec<u8>> {
    fetch(url, "image/*", max_bytes).await
}

/// The same, saying what kind of thing is wanted back.
///
/// The `Accept` header is the only thing that varies between the two callers —
/// an image for a message, JSON or a disk image for the updater — and a server
/// that content-negotiates should be told the truth about which.
pub async fn fetch(url: &str, accept: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let (status, body) = fetch_status(url, accept, max_bytes).await?;
    if !(200..300).contains(&status) {
        bail!("the server returned HTTP {status}");
    }
    Ok(body)
}

/// The same, handing back the status instead of judging it.
///
/// For the caller that can read a refusal: a 404 from an API asking for the
/// latest release is the honest answer that there is not one yet, and not
/// something to report as a failure.
pub async fn fetch_status(
    url: &str,
    accept: &str,
    max_bytes: usize,
) -> Result<(u16, Vec<u8>)> {
    let mut target = Url::parse(url)?;

    for _ in 0..=MAX_REDIRECTS {
        let response = get_once(&target, accept, max_bytes).await?;

        if let Some(location) = &response.redirect {
            target = resolve(&target, location)?;
            continue;
        }
        return Ok((response.status, response.body));
    }

    bail!("too many redirects")
}

struct RawResponse {
    status: u16,
    body: Vec<u8>,
    redirect: Option<String>,
}

async fn get_once(url: &Url, accept: &str, max_bytes: usize) -> Result<RawResponse> {
    let security = if url.secure { Security::Tls } else { Security::Plaintext };
    let mut stream = Stream::connect(&url.host, url.port, security).await?;

    let request = format!(
        "GET {target} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: spool/{version}\r\n\
         Accept: {accept}\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\
         \r\n",
        target = url.target,
        host = url.host,
        version = env!("CARGO_PKG_VERSION"),
    );

    stream
        .write_all(request.as_bytes())
        .await
        .with_context(|| format!("requesting {}", url.host))?;
    stream.flush().await.ok();

    // One byte over the cap is enough to know it was exceeded, and stopping
    // there means a hostile server cannot stream gigabytes into memory.
    let mut raw = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading from {}", url.host))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > max_bytes + MAX_HEADER_BYTES {
            bail!("the response is larger than {max_bytes} bytes");
        }
    }

    parse_binary(&raw, max_bytes)
}

/// Headers are allowed this much on top of the body budget.
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Everything a response carries, for a caller that needs all of it.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Headers a caller may not set, because they describe the framing rather
/// than the request.
///
/// `Host` is the important one. Where the request is *dialled* comes from the
/// URL, and an extension host checks that hostname against what the manifest
/// granted — so a caller able to set `Host` could be allowed to reach
/// `api.example.com` and served by a different virtual host at the same
/// address. The rest would make the message unparseable or let a body be
/// smuggled behind the one we counted.
const RESERVED: [&str; 4] = ["host", "content-length", "connection", "transfer-encoding"];

/// Builds the request head, or refuses to.
///
/// Separate from sending it so that what this refuses can be tested without a
/// server — and what it refuses is the interesting part, since the headers
/// come from an extension.
fn build_head(
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body_len: usize,
) -> Result<String> {
    let method = method.trim().to_ascii_uppercase();
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        bail!("`{method}` is not an HTTP method");
    }

    let mut head = format!(
        "{method} {target} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: spool/{version}\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n",
        target = url.target,
        host = url.host,
        version = env!("CARGO_PKG_VERSION"),
    );

    for (name, value) in headers {
        let name = name.trim();
        // A name or value carrying a newline is not a header, it is a second
        // request. Refused by name rather than stripped: a caller whose
        // authorization header was silently edited would debug the 401 for an
        // hour.
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':')
            || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
        {
            bail!("`{name}` is not a header this can send");
        }
        if RESERVED.iter().any(|reserved| name.eq_ignore_ascii_case(reserved)) {
            bail!("`{name}` is set by the client and cannot be overridden");
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }

    head.push_str(&format!("Content-Length: {body_len}\r\n\r\n"));
    Ok(head)
}

/// One request, with a method, headers and a body, and no redirects.
///
/// Redirects are *not* followed, unlike [`fetch`]. This is the call an
/// extension's `fetch` is served from, and its permission to make the request
/// at all was granted against the hostname in the URL. A 302 is a request to
/// go somewhere that was never granted, so it comes back as what it is — a
/// 302 — and the extension can decide what to do about it. Following it
/// silently would make the capability check advisory.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    max_bytes: usize,
) -> Result<Reply> {
    let url = Url::parse(url)?;
    let body = body.unwrap_or_default();
    let head = build_head(method, &url, headers, body.len())?;

    let security = if url.secure { Security::Tls } else { Security::Plaintext };
    let mut stream = Stream::connect(&url.host, url.port, security).await?;

    stream
        .write_all(head.as_bytes())
        .await
        .with_context(|| format!("requesting {}", url.host))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .await
            .with_context(|| format!("sending a body to {}", url.host))?;
    }
    stream.flush().await.ok();

    let mut raw = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading from {}", url.host))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > max_bytes + MAX_HEADER_BYTES {
            bail!("the response is larger than {max_bytes} bytes");
        }
    }

    let parsed = parse_binary(&raw, max_bytes)?;
    let (head, _) = split_head(&raw).context("the response had no header/body separator")?;

    Ok(Reply { status: parsed.status, headers: headers_of(&String::from_utf8_lossy(head)), body: parsed.body })
}

/// Every header on a response, in the order they arrived.
///
/// Names are lowercased because a caller comparing them should not have to
/// know whether this server writes `Content-Type` or `content-type`; values
/// are left exactly as they were sent.
fn headers_of(head: &str) -> Vec<(String, String)> {
    head.lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

/// The binary counterpart of [`parse_response`], which decodes to text.
///
/// Shares its head-splitting, chunk handling and header lookup, so the two
/// cannot drift on how a response is framed.
fn parse_binary(raw: &[u8], max_bytes: usize) -> Result<RawResponse> {
    let (head, body) = split_head(raw).context("the response had no header/body separator")?;
    let head = String::from_utf8_lossy(head);

    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("unreadable status line: {status_line:?}"))?;

    let body = if header(&head, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        dechunk(body).context("the response had a malformed chunked body")?
    } else if let Some(length) =
        header(&head, "content-length").and_then(|value| value.trim().parse::<usize>().ok())
    {
        body[..length.min(body.len())].to_vec()
    } else {
        body.to_vec()
    };

    if body.len() > max_bytes {
        bail!("the image is larger than {max_bytes} bytes");
    }

    let redirect = (300..400)
        .contains(&status)
        .then(|| header(&head, "location").map(str::to_string))
        .flatten();

    Ok(RawResponse { status, body, redirect })
}

/// Resolves a `Location` against the URL it came from.
///
/// Handles the three forms redirects actually use: absolute, protocol-relative
/// and relative to the current path.
fn resolve(base: &Url, location: &str) -> Result<Url> {
    let location = location.trim();
    if location.contains("://") {
        return Url::parse(location);
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = if base.secure { "https" } else { "http" };
        return Url::parse(&format!("{scheme}://{rest}"));
    }

    let target = match location.starts_with('/') {
        true => location.to_string(),
        false => {
            let directory = base.target.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            format!("{directory}/{location}")
        }
    };
    Ok(Url { target: target.split('#').next().unwrap_or("/").to_string(), ..base.clone() })
}

/// Splits a raw response into its status and body.
///
/// Separate from the socket so it is testable without a network, which is the
/// only way the malformed cases ever get exercised.
fn parse_response(raw: &[u8]) -> Result<Response> {
    // Split on bytes rather than on a lossy string: chunk lengths count bytes,
    // and replacing an invalid sequence with U+FFFD would shift every offset
    // after it.
    let (head, body) = split_head(raw).context("the response had no header/body separator")?;
    let head = String::from_utf8_lossy(head);

    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("unreadable status line: {status_line:?}"))?;

    let body = if header(&head, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        dechunk(body).context("the response had a malformed chunked body")?
    } else if let Some(length) =
        header(&head, "content-length").and_then(|value| value.trim().parse::<usize>().ok())
    {
        // Trust the header over what arrived, in case the connection carried
        // anything after the body.
        body[..length.min(body.len())].to_vec()
    } else {
        body.to_vec()
    };

    Ok(Response { status, body: String::from_utf8_lossy(&body).into_owned() })
}

/// Splits the head from the body at the first blank line.
fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(ix) = find(raw, b"\r\n\r\n") {
        return Some((&raw[..ix], &raw[ix + 4..]));
    }
    // A server that terminates headers with bare newlines is out of spec but
    // not worth failing over.
    find(raw, b"\n\n").map(|ix| (&raw[..ix], &raw[ix + 2..]))
}

/// Reads one header value, case-insensitively.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Decodes `Transfer-Encoding: chunked` into the bytes it was wrapping.
///
/// Each chunk is a hex length, then CRLF, then that many bytes, then CRLF; a
/// zero length ends the body. Truncation is an error rather than a partial
/// result — half a JSON document would only fail later, further from the cause.
fn dechunk(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());

    loop {
        let line_end = find(input, b"\r\n").context("a chunk length was never terminated")?;
        // Chunk extensions (`1a; name=value`) are legal and ignorable.
        let length_text = input[..line_end].split(|byte| *byte == b';').next().unwrap_or_default();
        let length_text = std::str::from_utf8(length_text)
            .context("a chunk length was not text")?
            .trim();

        let length = usize::from_str_radix(length_text, 16)
            .with_context(|| format!("unreadable chunk length: {length_text:?}"))?;
        input = &input[line_end + 2..];

        // The terminating zero-length chunk may be followed by trailers, which
        // this has no use for.
        if length == 0 {
            break;
        }
        if input.len() < length {
            bail!("a chunk claimed {length} bytes but only {} arrived", input.len());
        }

        out.extend_from_slice(&input[..length]);
        input = &input[length..];
        if input.starts_with(b"\r\n") {
            input = &input[2..];
        }
    }

    Ok(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Percent-encodes and joins form fields.
pub fn encode_form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes everything outside the unreserved set of RFC 3986.
///
/// Encoding more than strictly necessary is always safe; encoding less is not,
/// so this errs toward escaping. Space becomes `%20` rather than `+` — both
/// are accepted in a form body, and `%20` is also correct in a query string,
/// which lets the authorization URL reuse this.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Splits `a=1&b=2` into pairs, undoing both escaping conventions.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(part), String::new()),
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // A stray `%` is data, not a broken escape.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_click_post_is_never_sent_in_the_clear() {
        // Refused before anything is dialled, so this needs no network.
        let refused = smol::block_on(post_form_to("http://lists.example/u", &[("a", "b")]));
        assert!(refused.is_err());
    }

    #[test]
    fn a_normal_response_splits_into_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"a\":1}");
        assert!(response.is_success());
    }

    #[test]
    fn a_refusal_keeps_its_body() {
        // The reason an OAuth exchange failed is in the body of the 400, so
        // losing it would leave nothing to show the user.
        let raw = b"HTTP/1.1 400 Bad Request\r\n\r\n{\"error\":\"invalid_grant\"}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 400);
        assert!(!response.is_success());
        assert!(response.body.contains("invalid_grant"));
    }

    #[test]
    fn a_body_containing_a_blank_line_is_not_truncated() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\nfirst\r\n\r\nsecond";
        assert_eq!(parse_response(raw).unwrap().body, "first\r\n\r\nsecond");
    }

    #[test]
    fn a_headerless_response_is_an_error_rather_than_an_empty_body() {
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n").unwrap().body.is_empty());
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        // The shape Google's token endpoint actually sends. Read as-is, the
        // hex length lands in front of the JSON and serde reports it as a
        // number where a struct was expected.
        let raw = b"HTTP/1.1 200 OK\r\n\
                    Content-Type: application/json\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    14\r\n\
                    {\"access_token\":\"a\"}\r\n\
                    0\r\n\
                    \r\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"access_token\":\"a\"}");
    }

    #[test]
    fn a_body_split_across_several_chunks_is_rejoined() {
        let raw = b"HTTP/1.1 200 OK\r\n\
                    transfer-encoding: chunked\r\n\
                    \r\n\
                    5\r\nhello\r\n\
                    6\r\n world\r\n\
                    0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().body, "hello world");
    }

    #[test]
    fn chunk_extensions_are_ignored() {
        let raw = b"HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    5;name=value\r\nhello\r\n\
                    0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().body, "hello");
    }

    #[test]
    fn trailers_after_the_final_chunk_are_dropped() {
        let raw = b"HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    2\r\nok\r\n\
                    0\r\n\
                    X-Checksum: abc\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().body, "ok");
    }

    #[test]
    fn a_truncated_chunk_is_an_error_not_half_a_document() {
        let raw = b"HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    20\r\n{\"access_token\":";
        let error = match parse_response(raw) {
            Err(e) => format!("{e:#}"),
            Ok(response) => panic!("expected an error, got body {:?}", response.body),
        };
        assert!(error.contains("chunk"), "got: {error}");
    }

    #[test]
    fn content_length_trims_anything_after_the_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1} trailing junk";
        assert_eq!(parse_response(raw).unwrap().body, "{\"a\":1}");
    }

    #[test]
    fn a_content_length_longer_than_the_body_does_not_panic() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 9999\r\n\r\nshort";
        assert_eq!(parse_response(raw).unwrap().body, "short");
    }

    /// Hits Google's real token endpoint with a deliberately invalid code.
    ///
    /// Ignored by default because it needs the network. Fetching an image is
    /// the one path with no fixture that proves anything: redirects, chunked
    /// bodies and content sniffing all only show up against a live server.
    /// Run with `cargo test -p spool-net -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn a_real_image_url_comes_back_as_image_bytes() {
        let bytes = smol::block_on(get(
            "https://github.githubassets.com/favicons/favicon.png",
            8 * 1024 * 1024,
        ))
        .expect("the fetch should succeed");

        assert!(!bytes.is_empty());
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "expected PNG magic, got {:?}",
            &bytes[..8.min(bytes.len())]
        );
    }

    /// Ignored by default because it needs the network, but it is the only
    /// test that would have caught the bug it exists for: the endpoint chunks
    /// its response even when asked to close the connection, and no amount of
    /// hand-written fixture bytes proves what a live server actually sends.
    /// Run with `cargo test -p spool-net -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn google_token_endpoint_returns_parseable_json() {
        let response = smol::block_on(post_form(
            "oauth2.googleapis.com",
            "/token",
            &[("grant_type", "authorization_code"), ("code", "definitely-not-a-code")],
        ))
        .expect("the request itself should succeed");

        assert_eq!(response.status, 400, "a bogus code should be refused");
        // The point of the test: the body is JSON, not a hex length followed
        // by JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(&response.body).unwrap_or_else(|e| {
                panic!("body was not JSON ({e}): {:?}", response.body)
            });
        assert!(parsed.get("error").is_some(), "expected an OAuth error: {parsed}");
    }

    #[test]
    fn a_request_head_carries_what_it_was_given_and_frames_itself() {
        let url = Url::parse("https://api.example.com/v1/thing").unwrap();
        let headers = vec![
            ("authorization".to_string(), "Bearer sk-test".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];

        let head = build_head("post", &url, &headers, 17).unwrap();

        assert!(head.starts_with("POST /v1/thing HTTP/1.1\r\n"), "{head:?}");
        assert!(head.contains("\r\nHost: api.example.com\r\n"), "{head:?}");
        assert!(head.contains("\r\nauthorization: Bearer sk-test\r\n"), "{head:?}");
        assert!(head.ends_with("Content-Length: 17\r\n\r\n"), "{head:?}");
    }

    #[test]
    fn a_header_that_is_really_a_second_request_is_refused() {
        // The headers on this path come from an extension. A value carrying a
        // CRLF is not a header, it is everything after it.
        let url = Url::parse("https://api.example.com/").unwrap();

        let smuggled = vec![(
            "x-thing".to_string(),
            "fine\r\nX-Injected: not fine".to_string(),
        )];
        assert!(build_head("POST", &url, &smuggled, 0).is_err());

        let in_the_name = vec![("bad\r\nname".to_string(), "value".to_string())];
        assert!(build_head("POST", &url, &in_the_name, 0).is_err());

        let with_a_colon = vec![("x:thing".to_string(), "value".to_string())];
        assert!(build_head("POST", &url, &with_a_colon, 0).is_err());
    }

    #[test]
    fn the_headers_that_decide_where_a_request_lands_cannot_be_overridden() {
        // `Host` is the one that matters: permission to make the request was
        // granted against the hostname in the URL, and a caller able to set
        // this could be served by a different virtual host at that address.
        let url = Url::parse("https://api.example.com/").unwrap();

        for reserved in ["Host", "content-length", "Connection", "Transfer-Encoding"] {
            let headers = vec![(reserved.to_string(), "something".to_string())];
            let why = build_head("POST", &url, &headers, 0).unwrap_err().to_string();
            assert!(why.contains(reserved), "{reserved} was allowed through: {why}");
        }
    }

    #[test]
    fn a_method_that_is_not_one_is_refused() {
        let url = Url::parse("https://api.example.com/").unwrap();
        assert!(build_head("GET /other HTTP/1.1", &url, &[], 0).is_err());
        assert!(build_head("", &url, &[], 0).is_err());
        // And an ordinary one, spelled however it arrived.
        assert!(build_head("  get ", &url, &[], 0).unwrap().starts_with("GET /"));
    }

    #[test]
    fn response_headers_come_back_lowercased_with_their_values_intact() {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Rate-Limit: 4";
        let headers = headers_of(head);
        assert_eq!(headers[0], ("content-type".to_string(), "application/json".to_string()));
        assert_eq!(headers[1], ("x-rate-limit".to_string(), "4".to_string()));
    }

    #[test]
    fn headers_are_matched_case_insensitively() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nTRANSFER-Encoding: chunked";
        assert_eq!(header(head, "content-length"), Some("7"));
        assert_eq!(header(head, "Transfer-Encoding"), Some("chunked"));
        assert_eq!(header(head, "missing"), None);
        // The status line is not a header, and contains no colon to confuse it.
        assert_eq!(header(head, "HTTP/1.1"), None);
    }

    #[test]
    fn urls_split_into_the_parts_needed_to_dial() {
        let url = Url::parse("https://cdn.example.com/a/b.png?v=2#frag").unwrap();
        assert_eq!(url.host, "cdn.example.com");
        assert_eq!(url.port, 443);
        assert!(url.secure);
        // The fragment is a client-side concern and is never sent.
        assert_eq!(url.target, "/a/b.png?v=2");

        let url = Url::parse("http://example.com").unwrap();
        assert_eq!((url.port, url.secure, url.target.as_str()), (80, false, "/"));
        assert_eq!(Url::parse("http://example.com:8080/x").unwrap().port, 8080);
    }

    #[test]
    fn urls_that_cannot_be_dialled_are_refused() {
        assert!(Url::parse("ftp://example.com/a").is_err());
        assert!(Url::parse("example.com/a").is_err(), "a scheme is required");
        assert!(Url::parse("https:///nohost").is_err());
    }

    #[test]
    fn credentials_in_a_url_are_dropped_rather_than_sent() {
        let url = Url::parse("https://user:pass@example.com/a").unwrap();
        assert_eq!(url.host, "example.com");
        assert!(!url.target.contains("pass"));
    }

    #[test]
    fn redirects_resolve_in_all_three_forms_mail_uses() {
        let base = Url::parse("https://a.example.com/img/one.png").unwrap();

        assert_eq!(resolve(&base, "https://b.example.com/x.png").unwrap().host, "b.example.com");
        assert_eq!(resolve(&base, "//c.example.com/x.png").unwrap().host, "c.example.com");
        assert!(resolve(&base, "//c.example.com/x.png").unwrap().secure, "scheme carries over");

        let rooted = resolve(&base, "/other.png").unwrap();
        assert_eq!((rooted.host.as_str(), rooted.target.as_str()), ("a.example.com", "/other.png"));

        let relative = resolve(&base, "two.png").unwrap();
        assert_eq!(relative.target, "/img/two.png");
    }

    #[test]
    fn a_binary_response_keeps_its_bytes_intact() {
        // The reason this does not go through the text path: PNG bytes are not
        // UTF-8, and a lossy conversion would corrupt every image.
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00];
        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".to_vec();
        raw.extend_from_slice(&png);

        let response = parse_binary(&raw, 1024).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, png);
        assert_eq!(response.redirect, None);
    }

    #[test]
    fn a_redirect_surfaces_its_location() {
        let raw = b"HTTP/1.1 302 Found\r\nLocation: https://cdn.example.com/x.png\r\n\r\n";
        let response = parse_binary(raw, 1024).unwrap();
        assert_eq!(response.redirect.as_deref(), Some("https://cdn.example.com/x.png"));
    }

    #[test]
    fn an_oversized_body_is_refused_rather_than_kept() {
        let mut raw = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0u8; 64]);
        assert!(parse_binary(&raw, 16).is_err());
        assert!(parse_binary(&raw, 128).is_ok());
    }

    #[test]
    fn form_encoding_escapes_the_characters_that_would_split_a_field() {
        let encoded = encode_form(&[("scope", "a b&c=d"), ("code", "x/y+z")]);
        assert_eq!(encoded, "scope=a%20b%26c%3Dd&code=x%2Fy%2Bz");
    }

    #[test]
    fn the_unreserved_set_is_left_alone() {
        // Escaping these would be accepted by a server but makes a PKCE
        // verifier compare unequal to the one that was sent.
        let plain = "AZaz09-._~";
        assert_eq!(percent_encode(plain), plain);
    }

    #[test]
    fn non_ascii_is_encoded_as_utf8_bytes() {
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_decode("%C3%A9"), "é");
    }

    #[test]
    fn a_redirect_query_round_trips() {
        let pairs = parse_query("code=4%2F0AX&state=abc&scope=a+b");
        assert_eq!(pairs[0], ("code".into(), "4/0AX".into()));
        assert_eq!(pairs[1], ("state".into(), "abc".into()));
        // A browser sends `+` for a space in a query string.
        assert_eq!(pairs[2], ("scope".into(), "a b".into()));
    }

    #[test]
    fn a_malformed_query_does_not_panic() {
        assert!(parse_query("").is_empty());
        assert_eq!(parse_query("bare"), vec![("bare".to_string(), String::new())]);
        assert_eq!(parse_query("a=100%"), vec![("a".to_string(), "100%".to_string())]);
    }
}
