//! Composing: what a reply, a reply-all and a forward are made of.
//!
//! Everything here is a pure function over the message being answered, which
//! is deliberate — the rules for who a reply goes to are the part people
//! notice when a client gets them wrong, and they are testable only if no
//! network or database is involved in deciding them.
//!
//! Serializing a [`Draft`] to RFC 5322 bytes is `spool_net::compose`; this
//! module decides *what* to send, not how to encode it.

use crate::{Address, Envelope, MessageId, address, thread::normalize_subject};

/// A message being written.
///
/// Held by the composer and handed to the send path. `from` is filled in from
/// the account rather than chosen here, so a draft cannot name a sender the
/// account is not entitled to use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// What was written, as plain text — the whole of a plain message, and
    /// the text version of an HTML one. The composer edits this and only
    /// this; the HTML is made from it at send time (see [`html_body`]).
    pub body: String,
    /// Whether it goes as plain text or as HTML.
    pub format: Format,
    /// The message being answered or passed on, as HTML: sanitized, with its
    /// attribution or forwarding header, ready to go under what is written.
    ///
    /// Sent in place of the `> ` quote in [`Draft::body`] when the format is
    /// HTML, so a formatted message comes back formatted. The text quote
    /// stays in the body for the text version, and deleting it there is how
    /// this is left out as well.
    pub quoted_html: Option<String>,
    /// The body as edited in the HTML editor: what goes out as the HTML part,
    /// less the document around it. `None` until it has been edited that way,
    /// when the HTML is made from [`Draft::body`] instead.
    ///
    /// When it is set, [`Draft::body`] is the text read back out of it, so the
    /// two versions of the message say the same thing.
    pub html: Option<String>,

    /// The `Message-ID` this is answering, bare (no angle brackets).
    pub in_reply_to: Option<String>,
    /// The `References` chain, oldest first, bare.
    pub references: Vec<String>,

    /// The message this draft came from, so the UI can mark it answered once
    /// the send succeeds.
    pub replying_to: Option<MessageId>,
    /// The message this draft is passing on, so the UI can mark it forwarded
    /// once the send succeeds.
    ///
    /// Separate from [`Draft::replying_to`] because the two mean different
    /// things to the message they point at, and because a forward carries no
    /// threading headers — it deliberately starts a new conversation, so
    /// there is nothing else left on the draft that remembers where it came
    /// from.
    pub forwarding: Option<MessageId>,

    /// Files travelling with the message.
    ///
    /// Bytes rather than paths, and that is the load-bearing decision here. A
    /// draft is saved, the app is quit, and the draft is opened again a week
    /// later — a path by then may name a file that has moved, been deleted, or
    /// been edited into something the sender never meant to send. Bytes are
    /// also the only representation that serves both sources: a file picked
    /// from disk and a part carried along from a forwarded message arrive the
    /// same way and stop being distinguishable, which is what keeps everything
    /// downstream from needing to know which is which.
    pub attachments: Vec<Attached>,
}

/// How a message is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    /// Text and nothing else: the format no recipient's client can mangle.
    #[default]
    Plain,
    /// HTML, with a text version beside it for clients that want one.
    Html,
}

impl Format {
    /// As a draft is stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Plain => "plain",
            Format::Html => "html",
        }
    }

    /// Anything unrecognised is plain, which is always safe to send.
    pub fn parse(text: &str) -> Self {
        match text {
            "html" => Format::Html,
            _ => Format::Plain,
        }
    }
}

/// A file travelling with a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    /// The name the recipient sees. The file's own, with any directory part
    /// removed — sending the sender's directory layout along with the file is
    /// a small leak nobody intends.
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Attached {
    /// Names a file, guessing its type from its extension.
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let name = leaf_of(&name.into());
        Self { mime: mime_for(&name).to_string(), name, bytes }
    }

    /// What this part costs the message once encoded.
    ///
    /// Base64 is four characters for every three bytes, plus a line ending
    /// every 76 characters. Worth computing rather than approximating: the
    /// number is shown to somebody deciding whether their mail will be
    /// accepted, and "about a third bigger" is not a number they can act on.
    pub fn encoded_size(&self) -> usize {
        let groups = self.bytes.len().div_ceil(3);
        let characters = groups * 4;
        characters + characters.div_ceil(BASE64_LINE) * 2
    }
}

/// How many base64 characters go on a line, per RFC 2045.
const BASE64_LINE: usize = 76;

/// The total most providers stop accepting somewhere around.
///
/// Advisory, and deliberately not enforced. Spool does not know what the
/// server at the other end will take — Gmail stops at 25MB, other providers
/// are higher and a company's own server may be far lower — so this is the
/// point at which the composer says so, not the point at which it refuses.
/// Refusing to send something the server would have accepted is worse than
/// letting somebody find out.
pub const LARGE_MESSAGE: usize = 25 * 1024 * 1024;

/// The file name out of something that might be a path.
///
/// Both separators, because a name can arrive from a dropped file, a clipboard
/// image or a forwarded message, and only the first of those is certainly a
/// local path.
fn leaf_of(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).trim().to_string()
}

/// A content type from a file name.
///
/// A short table rather than a crate: this list is what mail actually carries,
/// and the answer for anything else — `application/octet-stream` — is both
/// correct and what every client does with a type it does not recognize.
/// Getting this wrong is not fatal; it decides whether the recipient's client
/// offers a preview.
pub fn mime_for(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());

    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("doc") => "application/msword",
        Some("docx") => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

impl Draft {
    /// A blank message from this account.
    pub fn new(from: Address) -> Self {
        Self { from, ..Default::default() }
    }

    /// Everyone who will receive this, for confirmation before sending.
    pub fn recipients(&self) -> Vec<&Address> {
        self.to.iter().chain(&self.cc).chain(&self.bcc).collect()
    }

    /// Why this draft is not ready to send, if it is not.
    ///
    /// Returned rather than enforced so the composer can grey out its button
    /// and say why, instead of failing at the SMTP layer where the reason
    /// arrives as a numeric code.
    pub fn problem(&self) -> Option<&'static str> {
        if self.from.addr.trim().is_empty() {
            return Some("no account to send from");
        }
        if self.recipients().is_empty() {
            return Some("no recipients");
        }
        if self.recipients().iter().any(|a| !is_plausible_address(&a.addr)) {
            return Some("a recipient address looks wrong");
        }
        None
    }

    pub fn is_sendable(&self) -> bool {
        self.problem().is_none()
    }

    /// What the attachments add to the message once encoded.
    pub fn attached_size(&self) -> usize {
        self.attachments.iter().map(Attached::encoded_size).sum()
    }

    /// Whether this is big enough to be worth warning about.
    ///
    /// Not part of [`Draft::problem`], on purpose. Size is a warning and never
    /// a refusal: what a server will accept is the server's business, and
    /// Spool refusing first would make a client that cannot send mail its own
    /// server would have taken.
    pub fn is_large(&self) -> bool {
        self.attached_size() + self.body.len() > LARGE_MESSAGE
    }
}

/// The loosest check worth making: something, an `@`, something with a dot.
///
/// Deliberately not an RFC 5322 validator. The grammar permits addresses no
/// human types and rejects none of the mistakes people actually make, so the
/// server's opinion is the one that counts — this only catches the typo that
/// would otherwise be discovered after the connection is open.
pub fn is_plausible_address(addr: &str) -> bool {
    let addr = addr.trim();
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };

    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !addr.contains(char::is_whitespace)
}

/// The message being answered.
///
/// Gathered by the caller rather than read out of an [`Envelope`], because an
/// envelope deliberately does not carry reply headers — it is the type held
/// once per message in an open mailbox, and a `Message-ID` per row is memory
/// spent on something only the message you actually reply to ever needs.
#[derive(Debug, Clone, Copy)]
pub struct Original<'a> {
    pub envelope: &'a Envelope,
    /// The original's own `Message-ID`, bare or bracketed.
    pub message_id: Option<&'a str>,
    /// Its `References` chain, oldest first.
    pub references: &'a [String],
    pub to: &'a [Address],
    pub cc: &'a [Address],
    /// Its `Reply-To`, which overrides the sender when present.
    pub reply_to: &'a [Address],
}

impl<'a> Original<'a> {
    /// The common case: everything the store hands back for an open message.
    pub fn new(envelope: &'a Envelope) -> Self {
        Self {
            envelope,
            message_id: None,
            references: &[],
            to: &[],
            cc: &[],
            reply_to: &[],
        }
    }

    /// This message's id, bare, or `None` when it never had one.
    fn bare_message_id(&self) -> Option<String> {
        let id = self.message_id?.trim().trim_matches(['<', '>']).trim();
        (!id.is_empty()).then(|| id.to_string())
    }
}

/// How a reply is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// Just the sender.
    Sender,
    /// The sender plus everyone else who was on it.
    All,
}

/// Builds a reply to `envelope`.
///
/// `me` is the account's own address, and is removed from the recipients —
/// replying-all to a thread you are on should not mail you a copy.
pub fn reply(original: Original<'_>, me: &Address, kind: ReplyKind, quoted_body: &str) -> Draft {
    let envelope = original.envelope;

    // `Reply-To` exists precisely to override the sender, and mailing lists
    // rely on it. When it is set it replaces the sender entirely.
    let mut to: Vec<Address> = if original.reply_to.is_empty() {
        vec![envelope.from.clone()]
    } else {
        original.reply_to.to_vec()
    };
    let mut cc = Vec::new();

    if kind == ReplyKind::All {
        // Everyone else on the original, minus whoever is already in `to`.
        cc.extend(original.to.iter().cloned());
        cc.extend(original.cc.iter().cloned());
    }

    dedupe(&mut to, &[]);
    dedupe(&mut cc, &to);

    // Never mail yourself a copy of your own reply.
    to.retain(|a| !same_address(a, me));
    cc.retain(|a| !same_address(a, me));

    // Replying to your own sent message has nobody left in `to`; the original
    // recipients are the right audience.
    if to.is_empty() {
        to = std::mem::take(&mut cc);
    }

    // The chain is the original's own references plus the original itself,
    // which is what lets every client in the thread file the reply correctly.
    let parent = original.bare_message_id();
    let mut references: Vec<String> = original.references.to_vec();
    if let Some(parent) = &parent
        && !references.iter().any(|r| r == parent)
    {
        references.push(parent.clone());
    }

    Draft {
        from: me.clone(),
        to,
        cc,
        bcc: Vec::new(),
        subject: reply_subject(&envelope.subject),
        body: quoted_body.to_string(),
        format: Format::Plain,
        quoted_html: None,
        html: None,
        in_reply_to: parent,
        references,
        replying_to: Some(envelope.id),
        forwarding: None,
        // A reply carries none of the original's files. Answering a message
        // is not resending it, and a client that quietly attached the
        // original's 8MB deck to every reply would be a client people stopped
        // replying from. A forward is the one that carries them.
        attachments: Vec::new(),
    }
}

/// Builds a forward. Recipients are left empty for the composer to fill.
pub fn forward(original: Original<'_>, me: &Address, quoted_body: &str) -> Draft {
    Draft {
        subject: forward_subject(&original.envelope.subject),
        body: quoted_body.to_string(),
        forwarding: Some(original.envelope.id),
        // A forward starts a new conversation rather than continuing the old
        // one: threading it into the original would file a stranger's copy
        // alongside the exchange it came from.
        ..Draft::new(me.clone())
    }
}

/// The signature delimiter: a line of exactly two hyphens and a space.
///
/// Every mail client recognises it, and most collapse what follows. The
/// trailing space is load-bearing and is why this is a constant rather than a
/// literal typed at each use — `--` without it is just a line of hyphens.
pub const SIGNATURE_DELIMITER: &str = "-- ";

/// Puts a signature into a draft body.
///
/// Above the quoted text, not at the very bottom: a reply is read from the
/// top, and a signature underneath the entire history of a thread is a
/// signature nobody ever sees.
pub fn sign(body: &str, signature: &str) -> String {
    let signature = signature.trim_end();
    if signature.is_empty() {
        return body.to_string();
    }
    let block = format!("{SIGNATURE_DELIMITER}\n{signature}");

    match body.strip_prefix("\n\n") {
        // A reply or a forward. The two blank lines at the top are where you
        // type; the signature goes under them and the quote stays below.
        Some(quoted) => format!("\n\n{block}\n\n{quoted}"),
        None if body.trim().is_empty() => format!("\n\n{block}"),
        None => format!("{body}\n\n{block}"),
    }
}

/// Swaps one signature block for another, when the old one is still intact.
///
/// `None` when there is nothing to swap — no old signature, or one that has
/// been typed over — and the caller then leaves the body exactly as it is.
/// That restraint is the whole point: changing which account a message is sent
/// from is not permission to rewrite what somebody has written, and a
/// signature that has been edited is writing.
pub fn resign(body: &str, was: &str, now: &str) -> Option<String> {
    let was = was.trim_end();
    let now = now.trim_end();
    if was.is_empty() || was == now {
        return None;
    }

    let old_block = format!("{SIGNATURE_DELIMITER}\n{was}");

    // Only the last one, and only where the block *ends* where the signature
    // does. Two reasons, and both are about not mangling text: a quoted reply
    // can carry the other side's signature, and a signature somebody has
    // added a line to still starts with the one that was put there — matching
    // that prefix and swapping it would leave their edit stuck to the new
    // account's name.
    let at = body
        .match_indices(&old_block)
        .filter(|(at, _)| {
            let after = &body[at + old_block.len()..];
            after.is_empty() || after.starts_with('\n')
        })
        .last()
        .map(|(at, _)| at)?;
    let mut swapped = String::with_capacity(body.len() + now.len());
    swapped.push_str(&body[..at]);
    match now.is_empty() {
        // The new account has no signature: the block goes rather than being
        // left as a bare delimiter with nothing under it.
        true => swapped.push_str(body[at + old_block.len()..].trim_start_matches('\n')),
        false => {
            swapped.push_str(&format!("{SIGNATURE_DELIMITER}\n{now}"));
            swapped.push_str(&body[at + old_block.len()..]);
        }
    }

    Some(swapped)
}

/// [`resign`], for a signature in HTML made by [`editor_content`]: swapped
/// when it is still as it was put there, and left alone once edited.
pub fn resign_html(html: &str, was: &str, now: &str) -> Option<String> {
    let block = |signature: &str| {
        let lines: Vec<String> = signature.trim_end().lines().map(crate::markup::escape).collect();
        lines.join("<br>\n")
    };
    let (was, now) = (was.trim_end(), now.trim_end());
    if was.is_empty() || was == now {
        return None;
    }
    let old = format!("<div class=\"spool-signature\">-- <br>\n{}</div>", block(was));
    if !html.contains(&old) {
        return None;
    }
    let new = match now.is_empty() {
        true => String::new(),
        false => format!("<div class=\"spool-signature\">-- <br>\n{}</div>", block(now)),
    };
    Some(html.replacen(&old, &new, 1))
}

/// `Re: ` unless it is already there.
///
/// Compared on the normalized subject so `RE:`, `Re:` and `Re: Re:` are all
/// recognized, which is the same rule threading uses.
pub fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return "Re:".into();
    }
    if normalize_subject(trimmed) != trimmed.to_lowercase() {
        // Something was stripped, so a prefix is already present.
        return trimmed.to_string();
    }
    format!("Re: {trimmed}")
}

pub fn forward_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return "Fwd:".into();
    }
    if trimmed.to_lowercase().starts_with("fwd:") || trimmed.to_lowercase().starts_with("fw:") {
        return trimmed.to_string();
    }
    format!("Fwd: {trimmed}")
}

/// The attribution line above quoted text.
pub fn attribution(envelope: &Envelope, date: &str) -> String {
    let who = if envelope.from.name.is_empty() {
        envelope.from.addr.clone()
    } else {
        format!("{} <{}>", envelope.from.name, envelope.from.addr)
    };
    format!("On {date}, {who} wrote:")
}

/// The HTML that goes under a reply: the attribution, then the original in a
/// quote. `original` is the original's HTML, already sanitized.
pub fn reply_quote_html(envelope: &Envelope, date: &str, original: &str) -> String {
    format!(
        "<div>{}</div>\n<blockquote {}>\n{}\n</blockquote>",
        crate::markup::escape(&attribution(envelope, date)),
        crate::markup::QUOTE_STYLE,
        crate::markup::body_of(original),
    )
}

/// The HTML that goes under a forward: the original's headers, then the
/// original as it was — not quoted, because a forward is passing it on.
pub fn forward_quote_html(envelope: &Envelope, date: &str, to: &[Address], original: &str) -> String {
    let escape = crate::markup::escape;
    let mut out = String::from("<div>---------- Forwarded message ----------<br>\n");
    out.push_str(&format!("From: {}<br>\n", escape(&envelope.from.full())));
    out.push_str(&format!("Date: {}<br>\n", escape(date)));
    out.push_str(&format!("Subject: {}<br>\n", escape(&envelope.subject)));
    if !to.is_empty() {
        out.push_str(&format!("To: {}<br>\n", escape(&address::encode_list(to))));
    }
    out.push_str("</div><br>\n");
    out.push_str(crate::markup::body_of(original));
    out
}

/// A draft's HTML, made at send time from what was written.
///
/// Three parts, top to bottom: the writing, marked up (see [`crate::markup`]);
/// the signature, kept as typed; and what is being answered — the original's
/// own HTML when there is one and its text quote is still in the body, else
/// that text quote, marked up like the rest.
pub fn html_body(draft: &Draft) -> String {
    let content = match &draft.html {
        Some(edited) => edited.clone(),
        None => editor_content(draft),
    };
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <style>blockquote{{margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex}}</style></head>\n\
         <body style=\"font-family:-apple-system,Helvetica,Arial,sans-serif;font-size:14px;line-height:1.45\">\n\
         {content}\n</body></html>\n"
    )
}

/// What the HTML editor starts with, made from what was written as text:
/// the writing, the signature and the quote, each in an element of its own so
/// the writing tools can find the part that is the author's.
pub fn editor_content(draft: &Draft) -> String {
    let body = &draft.body;
    let written = crate::assist::written_part(body);
    let rest = &body[written.end..];

    // The signature: after the delimiter, up to where the quote starts.
    let mut signature = Vec::new();
    let mut quote_at = rest.len();
    let mut offset = 0;
    let mut in_signature = false;
    for line in rest.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if !in_signature && text.trim_end() == SIGNATURE_DELIMITER.trim_end() {
            in_signature = true;
        } else if starts_quote(text) {
            quote_at = offset;
            break;
        } else if in_signature {
            signature.push(text);
        }
        offset += line.len();
    }
    while signature.last().is_some_and(|l| l.trim().is_empty()) {
        signature.pop();
    }
    let quote = rest[quote_at.min(rest.len())..].trim();

    let writing = crate::markup::to_html(&body[written]);
    // An empty writing area still needs a line to put the caret on.
    let writing = if writing.is_empty() { "<p style=\"margin:0 0 1em\"><br></p>".to_string() } else { writing };
    let mut html = format!("<div id=\"spool-writing\">{writing}</div>\n");
    html.push_str(&signature_html(&signature));
    if !quote.is_empty() {
        let quoted = match &draft.quoted_html {
            Some(original) => original.clone(),
            None => crate::markup::to_html(quote),
        };
        html.push_str(&format!("<br>\n<div id=\"spool-quote\">{quoted}</div>\n"));
    }
    html
}

/// A signature as the HTML part carries it: under the `-- ` line, as the text
/// one is, each of its lines a line. Nothing for no signature.
pub fn signature_html<S: AsRef<str>>(lines: &[S]) -> String {
    let mut lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = lines.iter().map(|l| crate::markup::escape(l)).collect();
    format!("<div class=\"spool-signature\">-- <br>\n{}</div>\n", lines.join("<br>\n"))
}

/// Whether a line is where the quoted or forwarded message starts: an
/// attribution, a quoted line, or a forwarding header.
fn starts_quote(line: &str) -> bool {
    line.starts_with('>')
        || line.trim_start().starts_with("---------- Forwarded message")
        || (line.starts_with("On ") && line.trim_end().ends_with("wrote:"))
}

/// Prefixes every line with `> `, the quoting convention every mail client
/// understands and this app's own reader already renders.
pub fn quote(body: &str) -> String {
    body.lines()
        .map(|line| if line.is_empty() { ">".to_string() } else { format!("> {line}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The full body of a reply: a blank line to type into, then the attribution
/// and the quoted original.
pub fn reply_body(envelope: &Envelope, date: &str, original: &str) -> String {
    format!("\n\n{}\n{}", attribution(envelope, date), quote(original))
}

/// The full body of a forward, which repeats the headers rather than quoting.
pub fn forward_body(
    envelope: &Envelope,
    date: &str,
    to: &[Address],
    original: &str,
) -> String {
    let mut out = String::from("\n\n---------- Forwarded message ----------\n");
    out.push_str(&format!("From: {}\n", envelope.from.full()));
    out.push_str(&format!("Date: {date}\n"));
    out.push_str(&format!("Subject: {}\n", envelope.subject));
    if !to.is_empty() {
        out.push_str(&format!("To: {}\n", address::encode_list(to)));
    }
    out.push('\n');
    out.push_str(original);
    out
}

fn same_address(a: &Address, b: &Address) -> bool {
    a.addr.trim().eq_ignore_ascii_case(b.addr.trim())
}

/// Removes duplicates within `list`, and anything already in `exclude`.
fn dedupe(list: &mut Vec<Address>, exclude: &[Address]) {
    let mut seen: Vec<String> = exclude.iter().map(|a| a.addr.to_lowercase()).collect();
    list.retain(|addr| {
        let key = addr.addr.trim().to_lowercase();
        if key.is_empty() || seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[test]
    fn a_file_is_typed_by_its_extension() {
        assert_eq!(Attached::new("report.pdf", vec![]).mime, "application/pdf");
        assert_eq!(Attached::new("shot.PNG", vec![]).mime, "image/png");
        assert_eq!(Attached::new("photo.jpeg", vec![]).mime, "image/jpeg");
        // Anything unrecognized, which every client handles as a download.
        assert_eq!(Attached::new("thing.xyzzy", vec![]).mime, "application/octet-stream");
        assert_eq!(Attached::new("Makefile", vec![]).mime, "application/octet-stream");
    }

    #[test]
    fn only_the_file_name_travels_with_the_file() {
        // Sending the sender's directory layout along with the attachment is
        // a small leak nobody intends, and some clients display the whole
        // string.
        assert_eq!(Attached::new("/Users/me/Desktop/q3.pdf", vec![]).name, "q3.pdf");
        assert_eq!(Attached::new("C:\\Users\\wes\\q3.pdf", vec![]).name, "q3.pdf");
        assert_eq!(Attached::new("  spaced.pdf  ", vec![]).name, "spaced.pdf");
    }

    #[test]
    fn the_encoded_size_is_what_the_message_actually_costs() {
        // Three bytes become four characters exactly, with no line ending
        // until 76 of them.
        assert_eq!(Attached::new("a.bin", vec![0; 3]).encoded_size(), 4 + 2);
        // A partial group is still padded to four.
        assert_eq!(Attached::new("a.bin", vec![0; 1]).encoded_size(), 4 + 2);
        assert_eq!(Attached::new("a.bin", vec![0; 57]).encoded_size(), 76 + 2);
        // Past one line, a second line ending.
        assert_eq!(Attached::new("a.bin", vec![0; 58]).encoded_size(), 80 + 4);
    }

    #[test]
    fn encoding_makes_a_file_about_a_third_bigger() {
        let megabyte = Attached::new("a.bin", vec![0; 1024 * 1024]);
        let ratio = megabyte.encoded_size() as f64 / megabyte.bytes.len() as f64;
        assert!((1.36..1.38).contains(&ratio), "{ratio}");
    }

    #[test]
    fn a_big_draft_is_warned_about_and_still_sendable() {
        // The decision this encodes: Spool does not know what the server will
        // take, so size is never a refusal.
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.to = vec![Address::new("You", "you@example.com")];
        assert!(!draft.is_large());

        draft.attachments = vec![Attached::new("huge.bin", vec![0; LARGE_MESSAGE])];
        assert!(draft.is_large());
        assert!(draft.is_sendable(), "big is a warning, never a refusal");
        assert_eq!(draft.problem(), None);
    }

    #[test]
    fn attached_size_adds_the_parts_up() {
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        assert_eq!(draft.attached_size(), 0);

        draft.attachments = vec![
            Attached::new("a.bin", vec![0; 3_000]),
            Attached::new("b.bin", vec![0; 6_000]),
        ];
        let expected: usize =
            draft.attachments.iter().map(Attached::encoded_size).sum();
        assert_eq!(draft.attached_size(), expected);
        assert!(draft.attached_size() > 9_000, "encoding is not free");
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn changing_account_swaps_the_signature_it_put_there() {
        let body = sign("Morning.", "Wes\nJoe Designs");
        let swapped = resign(&body, "Wes\nJoe Designs", "Wes\nFloatr").unwrap();

        assert!(swapped.starts_with("Morning."));
        assert!(swapped.contains("-- \nWes\nFloatr"));
        assert!(!swapped.contains("Joe Designs"));
    }

    #[test]
    fn a_signature_that_has_been_typed_over_is_left_alone() {
        let mut body = sign("Morning.", "Wes\nJoe Designs");
        body = body.replace("Joe Designs", "Joe Designs (mobile)");

        assert_eq!(resign(&body, "Wes\nJoe Designs", "Wes\nFloatr"), None);
    }

    #[test]
    fn an_account_with_no_signature_takes_the_block_away_with_it() {
        let body = sign("Morning.", "Wes");
        let swapped = resign(&body, "Wes", "").unwrap();

        assert!(!swapped.contains(SIGNATURE_DELIMITER), "{swapped:?}");
        assert!(swapped.starts_with("Morning."));
    }

    #[test]
    fn the_signature_being_replaced_is_this_writers_and_not_the_quoted_ones() {
        // A reply whose quoted text happens to carry the same signature block:
        // only the one at the bottom of what *this* composer wrote moves.
        let quoted = "\n\n-- \nWes\nJoe Designs\n\n> On Tuesday, someone wrote:\n> -- \n> Wes\n> Joe Designs";
        let swapped = resign(quoted, "Wes\nJoe Designs", "Wes\nFloatr").unwrap();

        assert!(swapped.contains("-- \nWes\nFloatr"));
        assert!(swapped.contains("> Joe Designs"), "the quote is untouched: {swapped:?}");
    }

    #[test]
    fn nothing_to_swap_is_not_a_change() {
        assert_eq!(resign("Morning.", "", "Wes"), None, "no old block to find");
        assert_eq!(resign("Morning.", "Wes", "Wes"), None, "the same signature");
        assert_eq!(resign("Morning.", "Wes", "Floatr"), None, "no block in the body");
    }
    use super::*;
    use crate::{AccountId, Flags, MailboxId, MessageId, ThreadId};

    #[test]
    fn a_signature_goes_under_the_space_you_type_in_not_under_the_quote() {
        let quoted = "\n\nOn Tuesday, Ana wrote:\n> hello";
        let signed = sign(quoted, "Wes\nstudio.example");

        assert_eq!(signed, "\n\n-- \nWes\nstudio.example\n\nOn Tuesday, Ana wrote:\n> hello");
        // The caret opens at the top, so what you write still lands above it.
        assert!(signed.starts_with("\n\n"));
        assert!(signed.find("-- ").unwrap() < signed.find("On Tuesday").unwrap());
    }

    #[test]
    fn a_new_message_gets_room_to_type_above_its_signature() {
        assert_eq!(sign("", "Wes"), "\n\n-- \nWes");
    }

    #[test]
    fn no_signature_leaves_the_body_exactly_as_it_was() {
        let quoted = "\n\nOn Tuesday, Ana wrote:\n> hello";
        assert_eq!(sign(quoted, ""), quoted);
        assert_eq!(sign(quoted, "   \n  "), quoted);
    }

    #[test]
    fn the_delimiter_keeps_its_trailing_space() {
        // Without it, other clients see a line of hyphens rather than a
        // signature, and none of them collapse it.
        assert_eq!(SIGNATURE_DELIMITER, "-- ");
        assert!(sign("", "Wes").contains("\n-- \n"));
    }

    fn addr(name: &str, email: &str) -> Address {
        Address::new(name, email)
    }

    fn envelope(from: Address, subject: &str) -> Envelope {
        Envelope {
            id: MessageId(1),
            account_id: AccountId(1),
            mailbox_id: MailboxId(1),
            thread_id: ThreadId(1),
            from,
            subject: subject.into(),
            date: 1_700_000_000,
            snippet: String::new(),
            flags: Flags::default(),
            has_attachments: false,
            size: 0,
            group: None,
        }
    }

    /// An `Original` carrying just an id, which is the usual shape.
    fn from_message<'a>(envelope: &'a Envelope, message_id: &'a str) -> Original<'a> {
        Original { message_id: Some(message_id), ..Original::new(envelope) }
    }

    fn me() -> Address {
        addr("Wes", "wes@example.com")
    }

    #[test]
    fn a_reply_goes_to_the_sender() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let draft = reply(from_message(&envelope, "abc@x"), &me(), ReplyKind::Sender, "");

        assert_eq!(draft.to, vec![addr("Ana", "ana@example.com")]);
        assert!(draft.cc.is_empty(), "a plain reply copies nobody");
        assert_eq!(draft.subject, "Re: Launch");
        assert_eq!(draft.in_reply_to.as_deref(), Some("abc@x"));
        assert_eq!(draft.references, vec!["abc@x"]);
        assert_eq!(draft.replying_to, Some(MessageId(1)));
    }

    #[test]
    fn reply_to_overrides_the_sender() {
        // What mailing lists rely on.
        let envelope = envelope(addr("Ana", "ana@example.com"), "List post");
        let list = vec![addr("Rust list", "rust@lists.example")];
        let original = Original { reply_to: &list, ..from_message(&envelope, "abc@x") };
        let draft = reply(original, &me(), ReplyKind::Sender, "");

        assert_eq!(draft.to, list);
    }

    #[test]
    fn reply_all_copies_everyone_else_but_not_me() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let to = vec![me(), addr("Bo", "bo@example.com")];
        let cc = vec![addr("Cy", "cy@example.com")];
        let original = Original { to: &to, cc: &cc, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::All, "");

        assert_eq!(draft.to, vec![addr("Ana", "ana@example.com")]);
        let copied: Vec<&str> = draft.cc.iter().map(|a| a.addr.as_str()).collect();
        assert_eq!(copied, vec!["bo@example.com", "cy@example.com"]);
        assert!(
            !draft.recipients().iter().any(|a| a.addr == "wes@example.com"),
            "replying should never mail me a copy of my own reply",
        );
    }

    #[test]
    fn reply_all_does_not_repeat_an_address_in_both_fields() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        // Ana is both the sender and on the To line, as happens constantly.
        let to = vec![addr("Ana", "ana@example.com"), addr("Bo", "bo@example.com")];
        let original = Original { to: &to, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::All, "");

        assert_eq!(draft.to, vec![addr("Ana", "ana@example.com")]);
        assert_eq!(draft.cc.len(), 1, "Ana must not also be copied: {:?}", draft.cc);
        assert_eq!(draft.cc[0].addr, "bo@example.com");
    }

    #[test]
    fn addresses_are_compared_case_insensitively() {
        let envelope = envelope(addr("Ana", "ANA@Example.com"), "Launch");
        let to = vec![addr("Ana again", "ana@example.com"), addr("Me", "WES@example.com")];
        let original = Original { to: &to, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::All, "");

        assert_eq!(draft.to.len(), 1);
        assert!(draft.cc.is_empty(), "case must not smuggle a duplicate through: {:?}", draft.cc);
    }

    #[test]
    fn replying_to_my_own_message_addresses_its_recipients() {
        // Otherwise "reply" to something in Sent has nobody to send to.
        let envelope = envelope(me(), "Launch");
        let to = vec![addr("Ana", "ana@example.com")];
        let original = Original { to: &to, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::All, "");

        assert_eq!(draft.to, vec![addr("Ana", "ana@example.com")]);
        assert!(draft.cc.is_empty());
    }

    #[test]
    fn subjects_gain_one_prefix_and_only_one() {
        assert_eq!(reply_subject("Launch"), "Re: Launch");
        assert_eq!(reply_subject("Re: Launch"), "Re: Launch");
        assert_eq!(reply_subject("RE: Launch"), "RE: Launch");
        assert_eq!(reply_subject("Re: Re: Launch"), "Re: Re: Launch");
        assert_eq!(reply_subject(""), "Re:");
        // "Regarding" is not a prefix.
        assert_eq!(reply_subject("Regarding launch"), "Re: Regarding launch");
    }

    #[test]
    fn forward_subjects_gain_fwd() {
        assert_eq!(forward_subject("Launch"), "Fwd: Launch");
        assert_eq!(forward_subject("Fwd: Launch"), "Fwd: Launch");
        assert_eq!(forward_subject("fw: Launch"), "fw: Launch");
        assert_eq!(forward_subject(""), "Fwd:");
    }

    #[test]
    fn a_forward_starts_a_new_conversation() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let draft = forward(from_message(&envelope, "abc@x"), &me(), "body");

        assert!(draft.to.is_empty(), "a forward has nobody to send to yet");
        assert_eq!(draft.in_reply_to, None);
        assert!(draft.references.is_empty(), "a forward is not a reply to the original");
        assert_eq!(draft.subject, "Fwd: Launch");
    }

    #[test]
    fn quoting_prefixes_every_line_including_blank_ones() {
        assert_eq!(quote("one\n\ntwo"), "> one\n>\n> two");
    }

    #[test]
    fn a_reply_body_leaves_room_to_type_above_the_quote() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let body = reply_body(&envelope, "12 March", "hello");

        assert!(body.starts_with("\n\n"), "the cursor belongs above the quote");
        assert!(body.contains("On 12 March, Ana <ana@example.com> wrote:"));
        assert!(body.contains("> hello"));
    }

    #[test]
    fn a_forward_body_repeats_the_headers() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let body = forward_body(&envelope, "12 March", &[addr("Bo", "bo@example.com")], "hi");

        assert!(body.contains("---------- Forwarded message ----------"));
        assert!(body.contains("From: Ana <ana@example.com>"));
        assert!(body.contains("Subject: Launch"));
        assert!(body.contains("To: Bo <bo@example.com>"));
        assert!(body.contains("hi"));
        assert!(!body.contains("> hi"), "a forward repeats rather than quotes");
    }

    #[test]
    fn the_references_chain_grows_by_one() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let chain = vec!["root@x".to_string(), "mid@x".to_string()];
        let original = Original { references: &chain, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::Sender, "");
        assert_eq!(draft.references, vec!["root@x", "mid@x", "abc@x"]);
    }

    #[test]
    fn a_parent_already_in_the_chain_is_not_repeated() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let chain = vec!["root@x".to_string(), "abc@x".to_string()];
        let original = Original { references: &chain, ..from_message(&envelope, "abc@x") };

        let draft = reply(original, &me(), ReplyKind::Sender, "");
        assert_eq!(draft.references, vec!["root@x", "abc@x"]);
    }

    #[test]
    fn a_message_with_no_id_still_produces_a_reply() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let draft = reply(Original::new(&envelope), &me(), ReplyKind::Sender, "");

        assert_eq!(draft.in_reply_to, None);
        assert!(draft.references.is_empty());
        assert_eq!(draft.to.len(), 1, "threading headers are optional; the reply is not");
    }

    #[test]
    fn angle_brackets_are_stripped_from_reply_headers() {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let draft = reply(from_message(&envelope, "<abc@x>"), &me(), ReplyKind::Sender, "");
        assert_eq!(draft.in_reply_to.as_deref(), Some("abc@x"));
    }

    #[test]
    fn a_draft_says_why_it_cannot_be_sent() {
        let mut draft = Draft::new(me());
        assert_eq!(draft.problem(), Some("no recipients"));

        draft.to = vec![addr("Bo", "not-an-address")];
        assert_eq!(draft.problem(), Some("a recipient address looks wrong"));

        draft.to = vec![addr("Bo", "bo@example.com")];
        assert_eq!(draft.problem(), None);
        assert!(draft.is_sendable());

        draft.from = Address::default();
        assert_eq!(draft.problem(), Some("no account to send from"));
    }

    #[test]
    fn plausible_addresses_admit_real_ones_and_reject_typos() {
        for good in ["a@b.co", "first.last+tag@sub.example.com", "x@y.example"] {
            assert!(is_plausible_address(good), "rejected {good}");
        }
        for bad in ["", "no-at-sign", "@example.com", "a@", "a@b", "a b@c.com", "a@b."] {
            assert!(!is_plausible_address(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn recipients_span_every_field() {
        let draft = Draft {
            to: vec![addr("A", "a@x.com")],
            cc: vec![addr("B", "b@x.com")],
            bcc: vec![addr("C", "c@x.com")],
            ..Draft::new(me())
        };
        assert_eq!(draft.recipients().len(), 3);
    }

    /// A reply to an HTML message, signed, as the composer builds it.
    fn html_reply() -> Draft {
        let envelope = envelope(addr("Ana", "ana@example.com"), "Launch");
        let quoted = reply_body(&envelope, "Monday", "Are we on for Thursday?");
        let mut draft = reply(from_message(&envelope, "abc@x"), &me(), ReplyKind::Sender, &quoted);
        draft.body = sign(&draft.body, "Wes\nJoe Designs");
        draft.body = format!("Yes, **Thursday** works.{}", draft.body.trim_start_matches('\n'));
        draft.body = draft.body.replacen("works.-- ", "works.\n\n-- ", 1);
        draft.format = Format::Html;
        draft.quoted_html = Some(reply_quote_html(
            &envelope,
            "Monday",
            "<html><head><style>p{}</style></head><body><p style=\"color:red\">Are we on?</p></body></html>",
        ));
        draft
    }

    #[test]
    fn an_html_reply_is_the_writing_the_signature_and_the_original_as_it_was() {
        let draft = html_reply();
        let html = html_body(&draft);

        let writing = html.find("<strong>Thursday</strong>").expect("the writing, marked up");
        let signature = html.find("spool-signature").expect("the signature");
        let original = html.find("<p style=\"color:red\">Are we on?</p>").expect("the original's own HTML");
        assert!(writing < signature && signature < original, "in that order: {html}");

        assert!(html.contains("Joe Designs"), "{html}");
        assert!(html.contains("On Monday, Ana &lt;ana@example.com&gt; wrote:"), "{html}");
        assert!(!html.contains("&gt; Are we on for Thursday"), "not the text quote as well: {html}");
        assert!(!html.contains("p{}"), "the original's stylesheet stays out of the reply: {html}");
    }

    #[test]
    fn deleting_the_quote_leaves_the_original_out_of_the_html_too() {
        let mut draft = html_reply();
        let cut = draft.body.find("On Monday").unwrap();
        draft.body.truncate(cut);
        let html = html_body(&draft);
        assert!(!html.contains("Are we on?"), "{html}");
        assert!(html.contains("<strong>Thursday</strong>"), "{html}");
    }

    #[test]
    fn with_no_original_html_the_text_quote_is_marked_up() {
        let mut draft = html_reply();
        draft.quoted_html = None;
        let html = html_body(&draft);
        assert!(html.contains("<blockquote"), "{html}");
        assert!(html.contains("Are we on for Thursday?"), "{html}");
    }

    #[test]
    fn what_was_edited_as_html_is_what_is_sent() {
        let mut draft = html_reply();
        draft.html = Some("<p>Edited <u>here</u></p>".into());
        let html = html_body(&draft);
        assert!(html.contains("<p>Edited <u>here</u></p>"), "{html}");
        assert!(!html.contains("Thursday"), "the text is not used once there is HTML: {html}");
    }

    #[test]
    fn the_editor_can_find_the_authors_part() {
        let draft = html_reply();
        let content = editor_content(&draft);
        let writing = content.find("id=\"spool-writing\"").expect("the writing is marked");
        let quote = content.find("id=\"spool-quote\"").expect("the quote is marked");
        assert!(writing < quote);

        let blank = editor_content(&Draft::new(me()));
        assert!(blank.contains("<br>"), "somewhere to put the caret: {blank}");
    }

    #[test]
    fn a_format_is_stored_by_name_and_anything_else_is_plain() {
        assert_eq!(Format::parse(Format::Html.as_str()), Format::Html);
        assert_eq!(Format::parse("plain"), Format::Plain);
        assert_eq!(Format::parse("rtf"), Format::Plain, "unknown is sent as text");
    }
}
