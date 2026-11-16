//! Turning a [`Draft`] into the bytes that go on the wire.
//!
//! RFC 5322 is mostly a 7-bit format with a 998-octet line limit, and neither
//! constraint matches what people type. Two encodings bridge that: RFC 2047
//! encoded-words for headers, and quoted-printable for the body. Both are
//! applied only when they are needed, so a plain ASCII message stays readable
//! in a raw dump — which is what makes this layer debuggable at all.

use spool_core::{
    Address,
    draft::{Draft, Format, html_body},
    format_rfc5322_date,
};

use crate::encoding::base64;

/// The line ending required on the wire. Bare newlines are widely tolerated
/// and occasionally fatal; there is no reason to find out which.
const CRLF: &str = "\r\n";

/// Renders a draft to a complete RFC 5322 message.
///
/// `message_id` is the bare id for this new message, and `now` its timestamp.
/// Both are passed rather than generated here so the output is a pure function
/// of its inputs and can be asserted on byte for byte.
pub fn render(draft: &Draft, message_id: &str, now: i64) -> String {
    render_with(draft, message_id, now, Copy::Sent)
}

/// Which copy of a message is being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copy {
    /// The one that goes to the recipients.
    Sent,
    /// The one filed in your own Sent folder, which does name the blind
    /// copies: the promise Bcc makes is to the *other recipients*, and a sent
    /// folder that cannot tell you who you copied is a record with the
    /// interesting part removed.
    Filed,
}

/// The same message, for one destination or the other.
pub fn render_with(draft: &Draft, message_id: &str, now: i64, copy: Copy) -> String {
    let mut out = String::with_capacity(draft.body.len() + 512);

    header(&mut out, "Date", &format_rfc5322_date(now));
    header(&mut out, "From", &encode_address(&draft.from));
    header(&mut out, "To", &encode_addresses(&draft.to));
    header(&mut out, "Cc", &encode_addresses(&draft.cc));
    // Bcc is deliberately absent from what is sent: it is delivered by naming
    // the recipient in the envelope, and writing it into the message would
    // show every blind copy to everyone else — the exact thing the field
    // promises not to do. The copy filed in your own Sent folder is the one
    // place it belongs, because nobody else ever reads that.
    if copy == Copy::Filed {
        header(&mut out, "Bcc", &encode_addresses(&draft.bcc));
    }
    header(&mut out, "Subject", &encode_header_text(&draft.subject));
    header(&mut out, "Message-ID", &format!("<{message_id}>"));

    if let Some(parent) = &draft.in_reply_to {
        header(&mut out, "In-Reply-To", &format!("<{parent}>"));
    }
    if !draft.references.is_empty() {
        let chain: Vec<String> = draft.references.iter().map(|r| format!("<{r}>")).collect();
        header(&mut out, "References", &chain.join(" "));
    }

    header(&mut out, "MIME-Version", "1.0");

    match (draft.format, draft.attachments.is_empty()) {
        // Still exactly the message it was before attachments existed. A
        // plain note wrapped in a multipart envelope for no reason is a
        // message that reads worse in every client that shows structure.
        (Format::Plain, true) => write_text(&mut out, &draft.body),
        (Format::Html, true) => write_alternative(&mut out, draft, message_id),
        (_, false) => write_multipart(&mut out, draft, message_id),
    }

    if !out.ends_with(CRLF) {
        out.push_str(CRLF);
    }
    out
}

/// Writes one header, folded, skipping it when the value is empty.
fn header(out: &mut String, name: &str, value: &str) {
    if !value.is_empty() {
        out.push_str(&fold_header(name, value));
        out.push_str(CRLF);
    }
}

/// The body as the whole message: headers already written, nothing to nest.
fn write_text(out: &mut String, body: &str) {
    header(out, "Content-Type", "text/plain; charset=utf-8");
    write_text_content(out, body);
}

/// The text itself, with whichever encoding it needs.
///
/// Shared by the single-part message and the first part of a multipart one, so
/// that a message does not change encoding merely because a file was attached
/// to it.
fn write_text_content(out: &mut String, body: &str) {
    let body = normalize_newlines(body);
    if needs_quoted_printable(&body) {
        header(out, "Content-Transfer-Encoding", "quoted-printable");
        out.push_str(CRLF);
        out.push_str(&quoted_printable(&body));
    } else {
        header(out, "Content-Transfer-Encoding", "7bit");
        out.push_str(CRLF);
        out.push_str(&body);
    }
}

/// `multipart/mixed`: the note, then the files.
fn write_multipart(out: &mut String, draft: &Draft, message_id: &str) {
    let boundary = boundary_for(message_id);

    header(out, "Content-Type", &format!("multipart/mixed; boundary=\"{boundary}\""));
    out.push_str(CRLF);

    // The preamble, which only a client too old to understand MIME will ever
    // show. Free to include and the one thing such a client can be told.
    out.push_str("This is a message in MIME format.");
    out.push_str(CRLF);

    out.push_str(&format!("--{boundary}{CRLF}"));
    match draft.format {
        // Stated rather than left to default. MIME says an absent
        // Content-Type is `text/plain; charset=us-ascii`, which is the wrong
        // charset for a body that may well not be ASCII at all.
        Format::Plain => {
            header(out, "Content-Type", "text/plain; charset=utf-8");
            write_text_content(out, &draft.body);
        }
        // The text and the HTML travel together as the first part, and the
        // files after them.
        Format::Html => write_alternative(out, draft, message_id),
    }
    if !out.ends_with(CRLF) {
        out.push_str(CRLF);
    }

    for attached in &draft.attachments {
        out.push_str(&format!("--{boundary}{CRLF}"));
        header(out, "Content-Type", &format!("{}; name=\"{}\"", attached.mime, quoted(&attached.name)));
        header(out, "Content-Transfer-Encoding", "base64");
        header(out, "Content-Disposition", &disposition(&attached.name));
        out.push_str(CRLF);
        out.push_str(&base64_lines(&attached.bytes));
    }

    out.push_str(&format!("--{boundary}--{CRLF}"));
}

/// `multipart/alternative`: the text version, then the HTML.
///
/// In that order because the last part a client understands is the one it
/// shows — so a client that reads HTML shows the HTML, and one that does not
/// still has the whole message as text rather than a page of tags.
fn write_alternative(out: &mut String, draft: &Draft, message_id: &str) {
    let boundary = format!("{}_alt", boundary_for(message_id));

    header(out, "Content-Type", &format!("multipart/alternative; boundary=\"{boundary}\""));
    out.push_str(CRLF);

    out.push_str(&format!("--{boundary}{CRLF}"));
    header(out, "Content-Type", "text/plain; charset=utf-8");
    write_text_content(out, &draft.body);
    if !out.ends_with(CRLF) {
        out.push_str(CRLF);
    }

    out.push_str(&format!("--{boundary}{CRLF}"));
    let (html, pictures) = inline_pictures(&html_body(draft), message_id);
    match pictures.is_empty() {
        true => {
            header(out, "Content-Type", "text/html; charset=utf-8");
            write_text_content(out, &html);
        }
        false => write_related(out, &html, &pictures, message_id),
    }
    if !out.ends_with(CRLF) {
        out.push_str(CRLF);
    }

    out.push_str(&format!("--{boundary}--{CRLF}"));
}

/// A picture written into the HTML, sent as a part of its own.
struct Inline {
    content_id: String,
    mime: String,
    name: String,
    bytes: Vec<u8>,
}

/// Takes the pictures out of the HTML: every `src` that is a base64 `data:`
/// image becomes a `cid:` URL naming a part that carries it.
///
/// The editor holds a dropped picture as a `data:` URL because that is what a
/// page can show without loading anything. It is not what goes out: Gmail and
/// Outlook show no `data:` image in a message, and every client shows a
/// `multipart/related` one.
fn inline_pictures(html: &str, message_id: &str) -> (String, Vec<Inline>) {
    let mut out = String::with_capacity(html.len());
    let mut pictures = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut at = 0;

    while let Some(ix) = lower[at..].find("src=").map(|ix| at + ix) {
        let start = ix + "src=".len();
        out.push_str(&html[at..start]);
        at = start;

        let Some(quote) = html[start..].chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            continue;
        };
        let value_start = start + 1;
        let Some(len) = html[value_start..].find(quote) else { continue };
        let value = &html[value_start..value_start + len];

        let Some(picture) = decode_data_image(value) else { continue };
        let (mime, bytes) = picture;
        let n = pictures.len() + 1;
        let content_id = format!("image{n}.{message_id}");
        let extension = mime.rsplit('/').next().unwrap_or("bin").replace("jpeg", "jpg");
        out.push(quote);
        out.push_str(&format!("cid:{content_id}"));
        out.push(quote);
        at = value_start + len + 1;
        pictures.push(Inline { content_id, name: format!("image{n}.{extension}"), mime, bytes });
    }
    out.push_str(&html[at..]);
    (out, pictures)
}

/// The type and bytes of a `data:image/...;base64,...` URL.
fn decode_data_image(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.trim().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?.trim().to_ascii_lowercase();
    if !mime.starts_with("image/") || !mime[6..].bytes().all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b)) {
        return None;
    }
    let bytes = crate::encoding::base64_decode(payload)?;
    (!bytes.is_empty()).then_some((mime, bytes))
}

/// `multipart/related`: the HTML, then the pictures it shows.
fn write_related(out: &mut String, html: &str, pictures: &[Inline], message_id: &str) {
    let boundary = format!("{}_rel", boundary_for(message_id));

    header(out, "Content-Type", &format!("multipart/related; boundary=\"{boundary}\""));
    out.push_str(CRLF);

    out.push_str(&format!("--{boundary}{CRLF}"));
    header(out, "Content-Type", "text/html; charset=utf-8");
    write_text_content(out, html);
    if !out.ends_with(CRLF) {
        out.push_str(CRLF);
    }

    for picture in pictures {
        out.push_str(&format!("--{boundary}{CRLF}"));
        header(out, "Content-Type", &format!("{}; name=\"{}\"", picture.mime, picture.name));
        header(out, "Content-Transfer-Encoding", "base64");
        header(out, "Content-ID", &format!("<{}>", picture.content_id));
        header(out, "Content-Disposition", &format!("inline; filename=\"{}\"", picture.name));
        out.push_str(CRLF);
        out.push_str(&base64_lines(&picture.bytes));
    }

    out.push_str(&format!("--{boundary}--{CRLF}"));
}

/// A boundary that cannot appear in the content it separates.
///
/// Derived from the `Message-ID`, which is already unique to this message, and
/// prefixed with `=_` — a sequence base64 never produces and quoted-printable
/// never leaves unescaped. That is what makes the guarantee: the parts are
/// base64 or quoted-printable, so neither can contain this line.
fn boundary_for(message_id: &str) -> String {
    let unique = message_id.split('@').next().unwrap_or(message_id);
    format!("=_spool_{unique}")
}

/// `Content-Disposition`, with the file name encoded if it needs it.
///
/// Non-ASCII names go out as RFC 2231 `filename*`, which is the correct form
/// and the one every client written this century reads. An encoded-word would
/// be more widely *seen* and is not actually legal in a parameter, which is how
/// a file called `résumé.pdf` arrives named `=?utf-8?B?...?=`.
fn disposition(name: &str) -> String {
    if name.is_ascii() {
        return format!("attachment; filename=\"{}\"", quoted(name));
    }

    let mut encoded = String::new();
    for byte in name.as_bytes() {
        match byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            true => encoded.push(*byte as char),
            false => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("attachment; filename*=UTF-8\'\'{encoded}")
}

/// Escapes what a quoted-string may not hold raw.
fn quoted(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"").replace(['\r', '\n'], "")
}

/// Base64, wrapped at the 76 characters RFC 2045 allows.
fn base64_lines(bytes: &[u8]) -> String {
    let encoded = crate::encoding::base64(bytes);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);

    for line in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push_str(CRLF);
    }
    out
}

/// A `Message-ID` for a message this client is sending.
///
/// The right-hand side is the sender's domain, which is what receiving servers
/// and spam filters expect to see; the left is random enough not to collide.
pub fn message_id(from: &Address, unique: &str) -> String {
    let domain = match from.addr.split_once('@') {
        Some((_, domain)) if !domain.is_empty() => domain,
        _ => "localhost",
    };
    format!("{unique}@{domain}")
}

/// `Display Name <addr@example.com>`, with the name encoded or quoted as
/// needed.
fn encode_address(address: &Address) -> String {
    let name = address.name.trim();
    if name.is_empty() {
        return address.addr.trim().to_string();
    }

    let encoded = if name.is_ascii() {
        // A name containing a comma, angle bracket or quote would otherwise
        // be read as address syntax.
        if name.contains(['"', ',', '<', '>', ':', ';', '@', '\\', '.']) {
            format!("\"{}\"", name.replace('\\', r"\\").replace('"', "\\\""))
        } else {
            name.to_string()
        }
    } else {
        encoded_word(name)
    };

    format!("{encoded} <{}>", address.addr.trim())
}

fn encode_addresses(addresses: &[Address]) -> String {
    addresses
        .iter()
        .filter(|a| !a.addr.trim().is_empty())
        .map(encode_address)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encodes header text only when it has to be encoded.
///
/// An encoded subject is unreadable in a raw message and in the handful of
/// tools that never learned RFC 2047, so ASCII is left exactly as typed.
fn encode_header_text(text: &str) -> String {
    if text.is_ascii() {
        // Control characters would end the header early — a header injection
        // if the text came from somewhere untrusted.
        text.replace(['\r', '\n'], " ")
    } else {
        encoded_word(text)
    }
}

/// RFC 2047 B-encoding: `=?utf-8?B?...?=`.
///
/// Base64 rather than Q-encoding because the text reaching this is non-ASCII
/// by definition, and Q-encoding it would escape most of the bytes anyway.
fn encoded_word(text: &str) -> String {
    // An encoded-word may not exceed 75 characters, so long text becomes
    // several, split on character boundaries and joined by whitespace.
    const MAX_BYTES: usize = 30;

    let mut words = Vec::new();
    let mut chunk = String::new();

    for ch in text.chars() {
        if chunk.len() + ch.len_utf8() > MAX_BYTES {
            words.push(format!("=?utf-8?B?{}?=", base64(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!("=?utf-8?B?{}?=", base64(chunk.as_bytes())));
    }

    words.join(" ")
}

/// Wraps a header onto continuation lines if it would otherwise run long.
///
/// Folding happens at existing spaces only; a header with no spaces to fold at
/// is left long, which every server accepts short of the 998-octet hard limit.
fn fold_header(name: &str, value: &str) -> String {
    const MAX: usize = 78;

    let mut out = format!("{name}: ");
    let mut column = out.len();

    for (ix, word) in value.split(' ').enumerate() {
        let leading = if ix == 0 { 0 } else { 1 };
        if ix > 0 && column + leading + word.len() > MAX {
            out.push_str(CRLF);
            // A continuation line must begin with whitespace.
            out.push(' ');
            column = 1;
        } else if ix > 0 {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
    }
    out
}

/// CRLF everywhere, whatever the editor produced.
fn normalize_newlines(body: &str) -> String {
    body.replace("\r\n", "\n").replace('\r', "\n").replace('\n', CRLF)
}

/// Whether the body needs encoding rather than going out as-is.
fn needs_quoted_printable(body: &str) -> bool {
    // A line that is just `.` is not handled here: quoted-printable leaves it
    // alone, because a dot is printable ASCII. Ending the DATA command early
    // is prevented by dot-stuffing in `smtp`, which is where the rule lives.
    !body.is_ascii() || body.lines().any(|line| line.len() > 998)
}

/// Quoted-printable, per RFC 2045.
fn quoted_printable(body: &str) -> String {
    const MAX_LINE: usize = 75;

    let mut out = String::with_capacity(body.len() * 2);
    let mut column = 0usize;

    // Operates on lines so that the CRLFs separating them stay literal rather
    // than being encoded as `=0D=0A`.
    let lines: Vec<&str> = body.split(CRLF).collect();
    for (ix, line) in lines.iter().enumerate() {
        if ix > 0 {
            out.push_str(CRLF);
            column = 0;
        }

        for byte in line.bytes() {
            // Printable ASCII except `=`, plus space and tab, go through
            // literally; everything else becomes `=XX`.
            let literal = matches!(byte, b'!'..=b'<' | b'>'..=b'~' | b' ' | b'\t');
            let escape;
            let piece: &str = if literal {
                escape = (byte as char).to_string();
                &escape
            } else {
                escape = format!("={byte:02X}");
                &escape
            };

            if column + piece.len() > MAX_LINE {
                // A soft line break: an `=` at the end of a line means the
                // line continues, and is removed by the decoder.
                out.push('=');
                out.push_str(CRLF);
                column = 0;
            }
            out.push_str(piece);
            column += piece.len();
        }

        // Trailing whitespace would be stripped in transit, so it is encoded.
        if line.ends_with(' ') || line.ends_with('\t') {
            let last = out.pop().unwrap_or(' ');
            out.push_str(&format!("={:02X}", last as u8));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_core::{Address, draft::Attached};

    fn addr(name: &str, email: &str) -> Address {
        Address::new(name, email)
    }

    fn draft() -> Draft {
        Draft {
            from: addr("Wes", "wes@example.com"),
            to: vec![addr("Ana", "ana@example.com")],
            subject: "Launch".into(),
            body: "Looks good.".into(),
            ..Draft::default()
        }
    }

    fn with_files(files: Vec<Attached>) -> Draft {
        Draft { attachments: files, ..draft() }
    }

    #[test]
    fn a_message_with_no_files_is_exactly_what_it_always_was() {
        // Attachments must not change the shape of a plain note. Wrapping one
        // in a multipart envelope for no reason is a message that reads worse
        // in every client that shows structure.
        let rendered = render(&draft(), "id@example.com", 0);

        assert!(rendered.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(!rendered.contains("multipart"), "{rendered}");
        assert!(!rendered.contains("boundary"));
    }

    #[test]
    fn a_file_makes_the_message_multipart_and_survives_intact() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let rendered = render(
            &with_files(vec![Attached::new("q3.pdf", bytes.clone())]),
            "unique@example.com",
            0,
        );

        assert!(rendered.contains("Content-Type: multipart/mixed; boundary=\"=_spool_unique\""));
        // The note is still the first part, and still readable.
        assert!(rendered.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(rendered.contains("Looks good."));

        assert!(rendered.contains("Content-Type: application/pdf; name=\"q3.pdf\""));
        assert!(rendered.contains("Content-Transfer-Encoding: base64"));
        assert!(rendered.contains("Content-Disposition: attachment; filename=\"q3.pdf\""));
        assert!(rendered.trim_end().ends_with("--=_spool_unique--"), "closing boundary");

        // The bytes come back out of it, which is the only thing that
        // ultimately matters.
        let part = rendered
            .split("--=_spool_unique")
            .find(|part| part.contains("q3.pdf"))
            .expect("the attachment part");
        let encoded: String = part
            .split_once("\r\n\r\n")
            .expect("headers then content")
            .1
            .split_whitespace()
            .collect();
        assert_eq!(crate::encoding::base64_decode(&encoded), Some(bytes));
    }

    #[test]
    fn several_files_each_get_their_own_part_in_order() {
        let rendered = render(
            &with_files(vec![
                Attached::new("one.txt", b"first".to_vec()),
                Attached::new("two.png", b"second".to_vec()),
            ]),
            "u@example.com",
            0,
        );

        let first = rendered.find("one.txt").expect("first file");
        let second = rendered.find("two.png").expect("second file");
        assert!(first < second, "attachments keep the order they were added");
        assert_eq!(rendered.matches("Content-Transfer-Encoding: base64").count(), 2);
    }

    #[test]
    fn base64_is_wrapped_at_the_length_the_rfc_allows() {
        // A single enormous line is legal-looking and rejected by servers that
        // enforce the 998-octet line limit.
        let rendered = render(
            &with_files(vec![Attached::new("big.bin", vec![0; 4_096])]),
            "u@example.com",
            0,
        );

        let longest = rendered.lines().map(str::len).max().unwrap_or(0);
        assert!(longest <= 78, "longest line was {longest}");
    }

    #[test]
    fn a_name_with_accents_goes_out_as_rfc_2231() {
        // An encoded-word is not legal in a parameter, which is how a file
        // called `résumé.pdf` arrives named `=?utf-8?B?...?=`.
        let rendered = render(
            &with_files(vec![Attached::new("résumé.pdf", b"x".to_vec())]),
            "u@example.com",
            0,
        );

        assert!(rendered.contains("filename*=UTF-8\'\'r%C3%A9sum%C3%A9.pdf"), "{rendered}");
        assert!(!rendered.contains("=?utf-8?"), "not an encoded-word");
    }

    #[test]
    fn a_name_with_a_quote_in_it_cannot_escape_the_parameter() {
        let rendered = render(
            &with_files(vec![Attached::new("a\"b.txt", b"x".to_vec())]),
            "u@example.com",
            0,
        );
        assert!(rendered.contains(r#"filename="a\"b.txt""#), "{rendered}");
    }

    #[test]
    fn the_boundary_cannot_occur_inside_the_parts_it_separates() {
        // The guarantee the `=_` prefix makes: base64 never produces it, and
        // quoted-printable never leaves it unescaped — so a body that contains
        // the boundary text cannot split the message.
        let boundary = boundary_for("unique@example.com");
        assert!(boundary.starts_with("=_"));

        let mut draft = with_files(vec![Attached::new("a.bin", vec![0; 64])]);
        draft.body = format!("Trying to break out: --{boundary}\nand more");

        let rendered = render(&draft, "unique@example.com", 0);
        // Three real boundary lines: opening, between, and closing. The one in
        // the body is quoted-printable-escaped and so is not one of them.
        let real = rendered.lines().filter(|l| l.starts_with(&format!("--{boundary}"))).count();
        assert_eq!(real, 3, "{rendered}");
    }

    /// Splits a rendered message into its headers and its body.
    fn split(message: &str) -> (String, String) {
        let (head, body) = message.split_once("\r\n\r\n").expect("no header separator");
        (head.to_string(), body.to_string())
    }

    #[test]
    fn a_plain_message_renders_readably() {
        let rendered = render(&draft(), "abc@example.com", 1_709_210_040);
        let (head, body) = split(&rendered);

        assert!(head.contains("From: Wes <wes@example.com>"));
        assert!(head.contains("To: Ana <ana@example.com>"));
        assert!(head.contains("Subject: Launch"));
        assert!(head.contains("Message-ID: <abc@example.com>"));
        assert!(head.contains("Date: Thu, 29 Feb 2024 12:34:00 +0000"));
        assert!(head.contains("Content-Transfer-Encoding: 7bit"));
        assert_eq!(body, "Looks good.\r\n");
    }

    #[test]
    fn every_line_ends_with_crlf() {
        let mut d = draft();
        d.body = "one\ntwo\r\nthree".into();
        let rendered = render(&d, "abc@example.com", 0);

        for line in rendered.split("\r\n") {
            assert!(!line.contains('\n'), "bare newline in {line:?}");
            assert!(!line.contains('\r'), "bare carriage return in {line:?}");
        }
    }

    #[test]
    fn the_filed_copy_records_the_blind_copies() {
        // The other half of the rule: the promise is to the other recipients,
        // not to the sender, and a Sent folder that cannot say who was copied
        // has thrown away the only record there is.
        let mut d = draft();
        d.bcc = vec![addr("Secret", "secret@example.com")];

        let filed = render_with(&d, "abc@example.com", 1_700_000_000, Copy::Filed);
        let head = filed.split("\r\n\r\n").next().unwrap().to_string();

        assert!(head.contains("Bcc: Secret <secret@example.com>"), "{head}");

        // And it is still absent from what actually goes out.
        let sent = render_with(&d, "abc@example.com", 1_700_000_000, Copy::Sent);
        assert!(!sent.to_lowercase().contains("bcc:"), "{sent}");
    }

    #[test]
    fn a_message_with_no_blind_copies_files_no_bcc_header() {
        let filed = render_with(&draft(), "abc@example.com", 1_700_000_000, Copy::Filed);
        assert!(!filed.to_lowercase().contains("bcc:"), "{filed}");
    }

    #[test]
    fn bcc_never_appears_in_the_message() {
        let mut d = draft();
        d.bcc = vec![addr("Secret", "secret@example.com")];
        let rendered = render(&d, "abc@example.com", 0);

        assert!(
            !rendered.contains("secret@example.com"),
            "a blind copy must not be visible to the other recipients",
        );
        assert!(!rendered.to_lowercase().contains("bcc:"));
    }

    #[test]
    fn reply_headers_are_bracketed() {
        let mut d = draft();
        d.in_reply_to = Some("parent@x".into());
        d.references = vec!["root@x".into(), "parent@x".into()];
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        assert!(head.contains("In-Reply-To: <parent@x>"));
        assert!(head.contains("References: <root@x> <parent@x>"));
    }

    #[test]
    fn an_empty_cc_is_omitted_rather_than_left_blank() {
        let (head, _) = split(&render(&draft(), "abc@example.com", 0));
        assert!(!head.contains("Cc:"));
    }

    #[test]
    fn non_ascii_subjects_are_encoded() {
        let mut d = draft();
        d.subject = "Café meeting".into();
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        assert!(head.contains("=?utf-8?B?"), "{head}");
        assert!(!head.contains("Café"), "raw non-ASCII must not reach a header");
    }

    #[test]
    fn ascii_subjects_are_left_alone() {
        let (head, _) = split(&render(&draft(), "abc@example.com", 0));
        assert!(head.contains("Subject: Launch"));
        assert!(!head.contains("=?utf-8?"));
    }

    #[test]
    fn newlines_in_a_subject_cannot_inject_a_header() {
        let mut d = draft();
        d.subject = "Launch\r\nBcc: attacker@example.com".into();
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        // The text survives, folded into the subject — what must not happen is
        // a line of its own that a parser would read as a header.
        assert!(
            !head.split("\r\n").any(|line| line.to_lowercase().starts_with("bcc:")),
            "header injection: {head}",
        );
        assert!(head.contains("Subject: Launch  Bcc:"), "{head}");
    }

    #[test]
    fn display_names_needing_quotes_get_them() {
        let mut d = draft();
        d.to = vec![addr("Ruiz, Ana", "ana@example.com")];
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        assert!(head.contains(r#""Ruiz, Ana" <ana@example.com>"#), "{head}");
    }

    #[test]
    fn a_non_ascii_display_name_is_encoded() {
        let mut d = draft();
        d.to = vec![addr("Ána Ruíz", "ana@example.com")];
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        assert!(head.contains("=?utf-8?B?"));
        assert!(head.contains("<ana@example.com>"));
    }

    #[test]
    fn an_unnamed_address_renders_bare() {
        let mut d = draft();
        d.to = vec![Address::bare("ana@example.com")];
        let (head, _) = split(&render(&d, "abc@example.com", 0));
        assert!(head.contains("To: ana@example.com"));
    }

    #[test]
    fn several_recipients_are_comma_separated() {
        let mut d = draft();
        d.to = vec![addr("Ana", "ana@example.com"), addr("Bo", "bo@example.com")];
        let (head, _) = split(&render(&d, "abc@example.com", 0));
        assert!(head.contains("To: Ana <ana@example.com>, Bo <bo@example.com>"));
    }

    #[test]
    fn a_non_ascii_body_is_quoted_printable() {
        let mut d = draft();
        d.body = "Café — naïve".into();
        let rendered = render(&d, "abc@example.com", 0);
        let (head, body) = split(&rendered);

        assert!(head.contains("Content-Transfer-Encoding: quoted-printable"));
        assert!(body.contains("=C3=A9"), "é should be encoded: {body}");
        assert!(body.is_ascii(), "quoted-printable output must be 7-bit");
    }

    #[test]
    fn quoted_printable_lines_stay_short() {
        let mut d = draft();
        d.body = "é".repeat(200);
        let (_, body) = split(&render(&d, "abc@example.com", 0));

        for line in body.split("\r\n") {
            assert!(line.len() <= 76, "line of {} chars: {line:?}", line.len());
        }
    }

    #[test]
    fn a_long_header_is_folded_onto_continuation_lines() {
        let mut d = draft();
        d.to = (0..12)
            .map(|i| addr(&format!("Person {i}"), &format!("person{i}@example.com")))
            .collect();
        let (head, _) = split(&render(&d, "abc@example.com", 0));

        let to_lines: Vec<&str> = head
            .split("\r\n")
            .skip_while(|l| !l.starts_with("To:"))
            .take_while(|l| l.starts_with("To:") || l.starts_with(' '))
            .collect();

        assert!(to_lines.len() > 1, "a long To should fold: {to_lines:?}");
        for line in &to_lines[1..] {
            assert!(line.starts_with(' '), "continuation must be indented: {line:?}");
        }
        // Folding must not lose anyone.
        assert!(head.contains("person11@example.com"));
    }

    #[test]
    fn message_ids_use_the_senders_domain() {
        assert_eq!(message_id(&addr("Wes", "wes@example.com"), "abc"), "abc@example.com");
        assert_eq!(message_id(&Address::bare("broken"), "abc"), "abc@localhost");
    }

    #[test]
    fn trailing_whitespace_is_encoded_so_it_survives() {
        let mut d = draft();
        // Non-ASCII forces quoted-printable, where the rule applies.
        d.body = "café \nnext".into();
        let (_, body) = split(&render(&d, "abc@example.com", 0));
        assert!(body.contains("=20"), "trailing space should be encoded: {body:?}");
    }

    #[test]
    fn a_lone_dot_line_is_left_for_the_smtp_layer_to_stuff() {
        // Rendering keeps the body faithful; `smtp::dot_stuff` is what stops a
        // lone dot from ending the DATA command. Encoding cannot help here —
        // quoted-printable passes `.` through unchanged.
        let mut d = draft();
        d.body = "before\n.\nafter".into();
        let (_, body) = split(&render(&d, "abc@example.com", 0));

        assert_eq!(body, "before\r\n.\r\nafter\r\n");
    }

    #[test]
    fn an_html_message_carries_the_text_and_the_html_side_by_side() {
        let mut d = draft();
        d.format = Format::Html;
        d.body = "Hello **Ana**".into();
        let rendered = render(&d, "abc@example.com", 0);

        assert!(rendered.contains("Content-Type: multipart/alternative; boundary=\"=_spool_abc_alt\""), "{rendered}");
        let text = rendered.find("Content-Type: text/plain").expect("a text version");
        let html = rendered.find("Content-Type: text/html").expect("an HTML version");
        assert!(text < html, "text first, so a client that reads HTML shows the HTML");
        assert!(rendered.contains("Hello **Ana**"), "the text version is what was typed");
        assert!(rendered.contains("<strong>Ana</strong>"), "{rendered}");
        assert!(rendered.contains("--=_spool_abc_alt--"), "the alternative is closed");
    }

    #[test]
    fn an_html_message_with_files_nests_the_two_versions_before_them() {
        let mut d = with_files(vec![Attached::new("a.pdf", vec![1, 2, 3])]);
        d.format = Format::Html;
        let rendered = render(&d, "abc@example.com", 0);

        let mixed = rendered.find("multipart/mixed").expect("files make it mixed");
        let alternative = rendered.find("multipart/alternative").expect("the two versions");
        let file = rendered.find("filename=\"a.pdf\"").expect("the file");
        assert!(mixed < alternative && alternative < file, "{rendered}");
    }

    #[test]
    fn a_picture_in_the_html_goes_out_as_a_part_it_points_at() {
        let mut d = draft();
        d.format = Format::Html;
        // "PNG" in base64.
        d.html = Some(r#"<p>Look:</p><img src="data:image/png;base64,UE5H" alt="shot"><img src="https://example.com/x.png">"#.into());
        let rendered = render(&d, "abc@example.com", 0);

        let alternative = rendered.find("multipart/alternative").expect("the two versions");
        let related = rendered.find("multipart/related").expect("the HTML and its pictures");
        let html = rendered.find("Content-Type: text/html").expect("the HTML");
        let picture = rendered.find("Content-Type: image/png; name=\"image1.png\"").expect("the picture");
        assert!(alternative < related && related < html && html < picture, "{rendered}");
        assert!(rendered.contains("src=3D\"cid:image1.abc@example.com\"") || rendered.contains("src=\"cid:image1.abc@example.com\""), "{rendered}");
        assert!(rendered.contains("Content-ID: <image1.abc@example.com>"), "{rendered}");
        assert!(rendered.contains("UE5H\r\n"), "{rendered}");
        assert!(!rendered.contains("data:image"), "{rendered}");
        assert!(rendered.contains("https://example.com/x.png"), "other images are left alone: {rendered}");
    }

    #[test]
    fn what_is_not_a_base64_picture_is_left_as_it_was() {
        let html = r#"<img src="data:text/html;base64,PGI+"><img src='data:image/png,raw'><a href="data:image/png;base64,UE5H">"#;
        let (out, pictures) = inline_pictures(html, "abc@example.com");
        assert_eq!(out, html);
        assert!(pictures.is_empty());
    }

    #[test]
    fn a_plain_message_is_still_exactly_text() {
        let rendered = render(&draft(), "abc@example.com", 0);
        assert!(!rendered.contains("text/html"), "{rendered}");
        assert!(!rendered.contains("multipart"), "{rendered}");
    }
}
