//! Converting a raw RFC 5322 message into something the store can hold.
//!
//! Parsing is done by `mail-parser`, which handles the parts of real-world
//! mail that are tedious and easy to get wrong: MIME nesting, transfer
//! encodings, RFC 2047 encoded words in headers, and the many ways a charset
//! can be declared or misdeclared. This module is only the translation layer
//! between its types and ours.

use spool_core::{
    AccountId, Address, Attachment, Flags, InlineImage, MailboxId, unsubscribe::ListUnsubscribe,
};
use spool_store::IncomingMessage;
use mail_parser::{Addr, Message, MessageParser, MimeHeaders};

/// Parses one message. Returns `None` only if the bytes are not a message at
/// all — a malformed body still yields whatever could be recovered, because
/// showing a broken message beats dropping it.
pub fn parse_message(
    raw: &[u8],
    account_id: AccountId,
    mailbox_id: MailboxId,
    uid: Option<u32>,
    flags: Flags,
) -> Option<IncomingMessage> {
    let parsed = MessageParser::default().parse(raw)?;

    let from = parsed
        .from()
        .and_then(|a| a.first())
        .map(to_address)
        .unwrap_or_default();

    let to = parsed.to().map(to_addresses).unwrap_or_default();
    let cc = parsed.cc().map(to_addresses).unwrap_or_default();
    // The header that decides where a reply goes. A mailing list sets it to
    // itself, and without it every reply to a list is a private message to
    // whoever happened to post.
    let reply_to = parsed.reply_to().map(to_addresses).unwrap_or_default();

    let attachments: Vec<Attachment> = parsed
        .attachments()
        .map(|part| Attachment {
            name: part
                .attachment_name()
                .map(str::to_string)
                .unwrap_or_else(|| "attachment".into()),
            mime: mime_of(part),
            size: part.len() as u32,
            content_id: part.content_id().map(spool_core::normalize_content_id),
            // A part with a Content-ID is made to be pointed at, whether or
            // not the sender said so — plenty of clients omit the disposition
            // header entirely. Whether anything does point at it is for the
            // reader to say (see `Body::draws`): Gmail gives every
            // attachment a Content-ID.
            inline: part
                .content_disposition()
                .is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline"))
                || part.content_id().is_some(),
        })
        .collect();

    // Attachment payloads, positionally matching `attachments`. They are free
    // here — sync already downloaded the whole message — so the only question
    // is disk, which the cap answers.
    let attachment_bytes: Vec<Vec<u8>> = parsed
        .attachments()
        .map(|part| {
            let bytes = part.contents();
            if bytes.len() > MAX_STORED_ATTACHMENT { Vec::new() } else { bytes.to_vec() }
        })
        .collect();

    let html = parsed.body_html(0).map(|c| c.into_owned()).unwrap_or_default();
    let inline_images = collect_inline_images(&parsed, &html);

    Some(IncomingMessage {
        account_id,
        mailbox_id,
        uid,
        rfc_message_id: parsed.message_id().unwrap_or_default().to_string(),
        in_reply_to: header_ids(parsed.in_reply_to()).into_iter().next().unwrap_or_default(),
        references: header_ids(parsed.references()),
        from,
        to,
        cc,
        reply_to,
        subject: parsed.subject().unwrap_or_default().to_string(),
        // A message with no parseable Date is filed at the epoch rather than
        // dropped; the list still shows it, just at the bottom.
        date: parsed.date().map(|d| d.to_timestamp()).unwrap_or(0),
        plain: parsed.body_text(0).map(|c| c.into_owned()).unwrap_or_default(),
        html,
        flags,
        attachments,
        inline_images,
        attachment_bytes,
        size: raw.len() as u32,
        list_unsubscribe: ListUnsubscribe::from_message(&parsed),
    })
}

/// The largest attachment worth copying into the database.
///
/// Sync downloads the whole message either way, so keeping the bytes costs
/// disk rather than bandwidth. Past this the trade stops being worth it: a
/// mailbox of 2,000 messages with 50 MB attachments would be a database
/// nobody wants, and the metadata still lands so the message can say what it
/// has and why it cannot be opened.
const MAX_STORED_ATTACHMENT: usize = 12 * 1024 * 1024;

/// The largest inline image worth keeping in the database.
///
/// Inline images are usually logos and signatures measured in kilobytes. A
/// part far past this is a photograph someone embedded, and copying it into
/// the message store would trade a lot of disk for one picture that the
/// attachment list already offers.
const MAX_INLINE_IMAGE: usize = 2 * 1024 * 1024;

/// Pulls out the image parts the HTML body actually refers to.
///
/// Referenced only: a message can carry a dozen parts with Content-IDs that
/// nothing points at, and storing those would mean paying for images no reader
/// will ever draw.
fn collect_inline_images(parsed: &Message<'_>, html: &str) -> Vec<InlineImage> {
    if html.is_empty() {
        return Vec::new();
    }

    let mut images = Vec::new();
    for part in parsed.attachments() {
        let Some(content_id) = part.content_id().map(spool_core::normalize_content_id) else {
            continue;
        };

        let mime = mime_of(part);
        if !mime.starts_with("image/") || part.len() > MAX_INLINE_IMAGE {
            continue;
        }
        if !references_cid(html, &content_id) {
            continue;
        }
        if images.iter().any(|i: &InlineImage| i.content_id == content_id) {
            continue;
        }

        images.push(InlineImage {
            content_id,
            mime,
            bytes: part.contents().to_vec(),
        });
    }
    images
}

/// Whether the HTML contains a `cid:` URL naming this part.
///
/// A substring search rather than a parse: it runs on every message during
/// sync, and the cost of a false positive is one small blob kept that nothing
/// draws.
fn references_cid(html: &str, content_id: &str) -> bool {
    let haystack = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(ix) = haystack[from..].find("cid:") {
        let start = from + ix + "cid:".len();
        let rest = &haystack[start..];
        let end = rest
            .find(['"', '\'', '>', ' ', ')'])
            .unwrap_or(rest.len());
        if spool_core::normalize_content_id(&rest[..end]) == content_id {
            return true;
        }
        from = start;
    }
    false
}

fn mime_of(part: &mail_parser::MessagePart<'_>) -> String {
    part.content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
        .to_ascii_lowercase()
}

fn to_address(addr: &Addr<'_>) -> Address {
    Address {
        name: addr.name().unwrap_or_default().to_string(),
        addr: addr.address().unwrap_or_default().to_string(),
    }
}

fn to_addresses(address: &mail_parser::Address<'_>) -> Vec<Address> {
    match address {
        mail_parser::Address::List(list) => list.iter().map(to_address).collect(),
        // A group — `Undisclosed recipients:;` and mailing lists — is
        // flattened to its members, which is what the UI wants to show.
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|group| group.addresses.iter())
            .map(to_address)
            .collect(),
    }
}

/// Pulls message-ids out of a `References` or `In-Reply-To` header, which may
/// arrive as one string, a list, or nothing at all.
fn header_ids(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    match value {
        mail_parser::HeaderValue::Text(text) => vec![normalize_id(text)],
        mail_parser::HeaderValue::TextList(list) => {
            list.iter().map(|t| normalize_id(t)).filter(|s| !s.is_empty()).collect()
        }
        _ => Vec::new(),
    }
}

/// Message-ids appear with and without angle brackets depending on the client
/// and the header. Threading compares them as strings, so they have to agree
/// on one form; this picks the bare form.
fn normalize_id(raw: &str) -> String {
    raw.trim().trim_start_matches('<').trim_end_matches('>').trim().to_string()
}

/// Maps IMAP flag names onto our packed representation.
pub fn flags_from_imap<'a>(names: impl Iterator<Item = &'a str>) -> Flags {
    let mut flags = Flags::default();
    for name in names {
        let bit = match name.trim_start_matches('\\').to_ascii_lowercase().as_str() {
            "seen" => Flags::SEEN,
            "flagged" => Flags::FLAGGED,
            "answered" => Flags::ANSWERED,
            "draft" => Flags::DRAFT,
            "deleted" => Flags::DELETED,
            // Not an IMAP flag but the keyword clients agree on for it; the
            // `\` trimmed above was never there on this one.
            "$forwarded" => Flags::FORWARDED,
            _ => continue,
        };
        flags = flags.with(bit, true);
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multipart/related message: HTML that points at an attached image.
    fn related(html: &str, cid: &str, disposition: &str) -> String {
        format!(
            "From: a@example.com\r\n\
             Subject: Test\r\n\
             Content-Type: multipart/related; boundary=B\r\n\
             \r\n\
             --B\r\n\
             Content-Type: text/html\r\n\
             \r\n\
             {html}\r\n\
             --B\r\n\
             Content-Type: image/png\r\n\
             Content-ID: <{cid}>\r\n\
             {disposition}\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             iVBORw0KGgo=\r\n\
             --B--\r\n"
        )
    }

    fn parse_raw(raw: &str) -> spool_store::IncomingMessage {
        parse_message(raw.as_bytes(), AccountId(1), MailboxId(1), Some(1), Flags::default())
            .expect("the message should parse")
    }

    fn parse_related(html: &str, cid: &str, disposition: &str) -> spool_store::IncomingMessage {
        parse_raw(&related(html, cid, disposition))
    }

    #[test]
    fn an_image_the_html_points_at_is_kept_with_its_bytes() {
        let msg = parse_related(
            r#"<p>hi</p><img src="cid:logo@example.com">"#,
            "logo@example.com",
            "Content-Disposition: inline\r\n",
        );

        assert_eq!(msg.inline_images.len(), 1);
        assert_eq!(msg.inline_images[0].content_id, "logo@example.com");
        assert_eq!(msg.inline_images[0].mime, "image/png");
        assert!(!msg.inline_images[0].bytes.is_empty(), "decoded from base64");

        let attachment = &msg.attachments[0];
        assert_eq!(attachment.content_id.as_deref(), Some("logo@example.com"));
        assert!(attachment.inline);
        assert!(attachment.is_embedded_image(), "shown in the body, not listed as a file");
    }

    #[test]
    fn a_content_id_nothing_refers_to_is_not_stored() {
        // Messages carry parts with Content-IDs that no `cid:` URL names;
        // keeping their bytes would be paying for images nothing draws.
        let msg = parse_related("<p>no images here</p>", "orphan@example.com", "");
        assert!(msg.inline_images.is_empty());
        // It is still listed as an attachment — it did arrive.
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(body_of(&msg).files().count(), 1);
    }

    /// What the reader is given for a parsed message, as far as the files go.
    fn body_of(msg: &spool_store::IncomingMessage) -> spool_core::Body {
        spool_core::Body {
            html: msg.html.clone(),
            attachments: msg.attachments.clone(),
            inline_images: msg.inline_images.clone(),
            ..Default::default()
        }
    }

    #[test]
    fn a_photo_sent_from_gmail_is_listed_as_a_file() {
        // Gmail's shape: the text and HTML, then the photo as an attachment
        // that also carries a Content-ID — which makes it look embedded —
        // and that the HTML never names.
        let raw = "From: a@example.com\r\n\
                   Subject: Holiday promo\r\n\
                   Content-Type: multipart/mixed; boundary=M\r\n\
                   \r\n\
                   --M\r\n\
                   Content-Type: multipart/alternative; boundary=A\r\n\
                   \r\n\
                   --A\r\n\
                   Content-Type: text/plain\r\n\
                   \r\n\
                   Here are the flyers\r\n\
                   --A\r\n\
                   Content-Type: text/html\r\n\
                   \r\n\
                   <div dir=\"auto\">Here are the flyers</div>\r\n\
                   --A--\r\n\
                   --M\r\n\
                   Content-Type: image/png; name=\"IMG_2256.png\"\r\n\
                   Content-Disposition: attachment; filename=\"IMG_2256.png\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   Content-ID: <1a140739ff24643128a1>\r\n\
                   X-Attachment-Id: 1a140739ff24643128a1\r\n\
                   \r\n\
                   iVBORw0KGgo=\r\n\
                   --M--\r\n";
        let msg = parse_raw(raw);
        assert!(msg.attachments[0].is_embedded_image(), "it looks embedded");
        assert!(msg.inline_images.is_empty(), "nothing draws it");

        let body = body_of(&msg);
        let files: Vec<&str> = body.files().map(|(_, a)| a.name.as_str()).collect();
        assert_eq!(files, ["IMG_2256.png"]);
    }

    #[test]
    fn an_image_too_large_to_draw_is_listed_as_a_file() {
        // Named by the HTML, but past what the store keeps to draw: shown
        // nowhere unless it is offered as a file.
        let large = "A".repeat((MAX_INLINE_IMAGE + 3) / 3 * 4 + 4);
        let raw = related(r#"<img src="cid:photo@example.com">"#, "photo@example.com", "")
            .replace("iVBORw0KGgo=", &large);
        let msg = parse_raw(&raw);
        assert!(msg.inline_images.is_empty());
        assert_eq!(body_of(&msg).files().count(), 1);
    }

    #[test]
    fn an_image_the_html_draws_is_not_listed_as_a_file() {
        let msg = parse_related(
            r#"<img src="cid:logo@example.com">"#,
            "logo@example.com",
            "Content-Disposition: inline\r\n",
        );
        let body = body_of(&msg);
        assert!(body.draws(&body.attachments[0]));
        assert_eq!(body.files().count(), 0);
    }

    #[test]
    fn a_part_with_a_content_id_counts_as_inline_without_the_disposition_header() {
        // Plenty of clients omit Content-Disposition entirely.
        let msg = parse_related(
            r#"<img src="cid:sig@example.com">"#,
            "sig@example.com",
            "",
        );
        assert!(msg.attachments[0].inline);
        assert_eq!(msg.inline_images.len(), 1);
    }

    #[test]
    fn cid_references_are_matched_regardless_of_bracketing_and_case() {
        assert!(references_cid(r#"<img src="cid:Logo@Example.com">"#, "logo@example.com"));
        assert!(references_cid(r#"<img src='cid:<logo@example.com>'>"#, "logo@example.com"));
        assert!(references_cid("url(cid:logo@example.com)", "logo@example.com"));

        // A different id, and an id that is only a prefix of the reference.
        assert!(!references_cid(r#"<img src="cid:other@example.com">"#, "logo@example.com"));
        assert!(!references_cid(r#"<img src="cid:logo@example.com.br">"#, "logo@example.com"));
        assert!(!references_cid("no images", "logo@example.com"));
    }

    #[test]
    fn a_message_with_no_html_stores_no_inline_images() {
        // Nothing can reference them, so there is nothing to keep.
        let raw = "From: a@example.com\r\n\
                   Content-Type: multipart/mixed; boundary=B\r\n\
                   \r\n--B\r\n\
                   Content-Type: text/plain\r\n\r\nhi\r\n--B\r\n\
                   Content-Type: image/png\r\n\
                   Content-ID: <x@y>\r\n\r\nbytes\r\n--B--\r\n";
        assert!(parse_raw(raw).inline_images.is_empty());
    }

    const SAMPLE: &[u8] = b"\
From: Ana Ruiz <ana@example.com>\r
To: Wes <wes@example.com>, Bo <bo@example.com>\r
Cc: cc@example.com\r
Subject: Re: Launch plan\r
Date: Tue, 12 Mar 2024 09:30:00 +0000\r
Message-ID: <child@example.com>\r
In-Reply-To: <parent@example.com>\r
References: <root@example.com> <parent@example.com>\r
Content-Type: text/plain; charset=utf-8\r
\r
Looks good to me.\r
";

    fn parse(raw: &[u8]) -> IncomingMessage {
        parse_message(raw, AccountId(1), MailboxId(2), Some(7), Flags::default()).unwrap()
    }

    #[test]
    fn headers_and_body_are_extracted() {
        let msg = parse(SAMPLE);

        assert_eq!(msg.from, Address::new("Ana Ruiz", "ana@example.com"));
        assert_eq!(msg.to.len(), 2);
        assert_eq!(msg.to[1].addr, "bo@example.com");
        assert_eq!(msg.cc[0].addr, "cc@example.com");
        assert_eq!(msg.subject, "Re: Launch plan");
        assert_eq!(msg.uid, Some(7));
        assert!(msg.plain.contains("Looks good to me."));
        assert_eq!(msg.date, 1_710_235_800, "2024-03-12T09:30:00Z");
    }

    #[test]
    fn reply_headers_are_normalized_to_bare_ids() {
        let msg = parse(SAMPLE);
        assert_eq!(msg.rfc_message_id, "child@example.com");
        assert_eq!(msg.in_reply_to, "parent@example.com");
        assert_eq!(msg.references, vec!["root@example.com", "parent@example.com"]);
    }

    #[test]
    fn reply_to_is_extracted_so_a_list_reply_reaches_the_list() {
        // The failure this prevents: answering a mailing list and having the
        // message go privately to whoever happened to post.
        let raw = b"From: Ana Ruiz <ana@example.com>\r\n\
                    Reply-To: Rust Users <users@lists.example.org>\r\n\
                    To: users@lists.example.org\r\n\
                    Subject: A question\r\n\
                    Message-ID: <q@example.com>\r\n\
                    \r\n\
                    Body.\r\n";
        let msg = parse_message(raw, AccountId(1), MailboxId(1), None, Flags::default()).unwrap();

        assert_eq!(msg.reply_to.len(), 1);
        assert_eq!(msg.reply_to[0].addr, "users@lists.example.org");
        assert_eq!(msg.reply_to[0].name, "Rust Users");
        assert_eq!(msg.from.addr, "ana@example.com", "and the sender is still recorded");
    }

    #[test]
    fn a_message_without_reply_to_records_none() {
        assert!(parse(SAMPLE).reply_to.is_empty());
    }

    #[test]
    fn encoded_words_in_headers_are_decoded() {
        let raw = b"\
From: =?utf-8?B?w4FuYSBSdcOteg==?= <ana@example.com>\r
Subject: =?utf-8?q?Caf=C3=A9_meeting?=\r
\r
body\r
";
        let msg = parse(raw);
        assert_eq!(msg.subject, "Café meeting");
        assert_eq!(msg.from.name, "Ána Ruíz");
    }

    #[test]
    fn multipart_alternative_yields_both_bodies() {
        let raw = b"\
From: a@example.com\r
Subject: Both\r
Content-Type: multipart/alternative; boundary=\"b\"\r
\r
--b\r
Content-Type: text/plain\r
\r
plain version\r
--b\r
Content-Type: text/html\r
\r
<p>html version</p>\r
--b--\r
";
        let msg = parse(raw);
        assert!(msg.plain.contains("plain version"));
        assert!(msg.html.contains("html version"));
    }

    #[test]
    fn attachments_are_listed_with_name_and_type() {
        let raw = b"\
From: a@example.com\r
Subject: With file\r
Content-Type: multipart/mixed; boundary=\"b\"\r
\r
--b\r
Content-Type: text/plain\r
\r
see attached\r
--b\r
Content-Type: application/pdf; name=\"q3.pdf\"\r
Content-Disposition: attachment; filename=\"q3.pdf\"\r
\r
%PDF-1.4 fake\r
--b--\r
";
        let msg = parse(raw);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].name, "q3.pdf");
        assert_eq!(msg.attachments[0].mime, "application/pdf");
    }

    #[test]
    fn a_message_with_no_date_still_parses() {
        let msg = parse(b"From: a@example.com\r\nSubject: No date\r\n\r\nbody\r\n");
        assert_eq!(msg.date, 0);
        assert_eq!(msg.subject, "No date");
    }

    #[test]
    fn groups_are_flattened_to_their_members() {
        let raw = b"\
From: a@example.com\r
To: Team: bo@example.com, cy@example.com;\r
Subject: Group\r
\r
body\r
";
        let msg = parse(raw);
        let addrs: Vec<&str> = msg.to.iter().map(|a| a.addr.as_str()).collect();
        assert_eq!(addrs, vec!["bo@example.com", "cy@example.com"]);
    }

    #[test]
    fn imap_flags_map_onto_our_bits() {
        let flags = flags_from_imap(["\\Seen", "\\Flagged", "$label1"].into_iter());
        assert!(flags.is_seen());
        assert!(flags.is_flagged());
        assert!(!flags.is_answered(), "unknown keywords are ignored");
    }
}
