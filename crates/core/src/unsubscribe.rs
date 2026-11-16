//! Leaving a mailing list: `List-Unsubscribe` (RFC 2369) and the one-click
//! form of it (RFC 8058).
//!
//! A list names one or more ways out in angle brackets — usually a `mailto:`
//! and an `https:` link. A sender that also sends
//! `List-Unsubscribe-Post: List-Unsubscribe=One-Click` promises that a bare
//! POST to the link is the whole job, with no page to click through. That
//! promise is what makes an unsubscribe button possible without a browser, and
//! it is the one this prefers.
//!
//! Only the header's text is stored. Deciding what to do with it happens here,
//! when somebody asks, so a better reading of the header later applies to
//! every message already in the store.

/// What a message says about leaving the list it came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListUnsubscribe {
    /// Nobody has looked. Mail stored before these headers were kept is in
    /// this state until the server is asked.
    #[default]
    Unknown,
    /// The message has no `List-Unsubscribe`.
    Absent,
    /// The header as it arrived, unfolded, and whether the sender offered
    /// one-click.
    Offered { header: String, one_click: bool },
}

/// How to carry out an unsubscribe, most preferable first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// POST `List-Unsubscribe=One-Click` to this `https:` URL, and that is
    /// the end of it.
    OneClick(String),
    /// Send a message to this address.
    Mail { to: String, subject: String, body: String },
    /// Open this page, where the reader finishes the job themselves.
    Browser(String),
}

/// The body RFC 8058 says a one-click POST carries.
pub const ONE_CLICK_BODY: (&str, &str) = ("List-Unsubscribe", "One-Click");

impl ListUnsubscribe {
    /// Reads the two headers, given their raw values.
    ///
    /// An empty or absent `List-Unsubscribe` is [`ListUnsubscribe::Absent`]:
    /// once the headers have been read, "none" is a known answer.
    pub fn from_headers(unsubscribe: Option<&str>, post: Option<&str>) -> Self {
        let header = unfold(unsubscribe.unwrap_or_default());
        if header.is_empty() {
            return Self::Absent;
        }
        let one_click = post.is_some_and(|post| {
            let post: String = post.chars().filter(|c| !c.is_whitespace()).collect();
            post.eq_ignore_ascii_case("List-Unsubscribe=One-Click")
        });
        Self::Offered { header, one_click }
    }

    /// Reads them off a parsed message.
    pub fn from_message(message: &mail_parser::Message<'_>) -> Self {
        Self::from_headers(
            message.header_raw("List-Unsubscribe"),
            message.header_raw("List-Unsubscribe-Post"),
        )
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The best way out this message offers, or `None` when it offers none
    /// that can be used.
    ///
    /// One-click first, because it needs nothing from the reader and tells the
    /// sender nothing a click would not. Mail next, because it too finishes
    /// without a web page. A plain link last: it only takes the reader
    /// somewhere they still have to do it themselves.
    pub fn method(&self) -> Option<Method> {
        let Self::Offered { header, one_click } = self else { return None };
        let uris = uris(header);

        let web = |secure_only: bool| {
            uris.iter().find(|uri| {
                let lower = uri.to_ascii_lowercase();
                lower.starts_with("https://") || (!secure_only && lower.starts_with("http://"))
            })
        };

        if *one_click && let Some(url) = web(true) {
            return Some(Method::OneClick(url.clone()));
        }
        if let Some(mail) = uris.iter().find_map(|uri| mailto(uri)) {
            return Some(mail);
        }
        web(false).map(|url| Method::Browser(url.clone()))
    }
}

impl Method {
    /// Who is being asked, for a sentence about it: the host or the address.
    pub fn target(&self) -> String {
        match self {
            Self::OneClick(url) | Self::Browser(url) => host_of(url).to_string(),
            Self::Mail { to, .. } => to.clone(),
        }
    }
}

/// Joins a folded header back onto one line.
fn unfold(value: &str) -> String {
    value.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The bracketed URIs in a `List-Unsubscribe` value, in order.
///
/// Text outside the brackets is comment and ignored. Whitespace inside them is
/// removed: RFC 2369 lets a long URL be folded across lines, and a folded URL
/// with a space left in it is a different URL.
fn uris(header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = header;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start + 1..].find('>') else { break };
        let uri: String =
            rest[start + 1..start + 1 + len].chars().filter(|c| !c.is_whitespace()).collect();
        if !uri.is_empty() {
            out.push(uri);
        }
        rest = &rest[start + 1 + len + 1..];
    }
    out
}

/// A `mailto:` URI (RFC 6068) as a message to send, if it names an address.
fn mailto(uri: &str) -> Option<Method> {
    let scheme = uri.get(..7)?;
    if !scheme.eq_ignore_ascii_case("mailto:") {
        return None;
    }
    let (to, query) = match uri[7..].split_once('?') {
        Some((to, query)) => (to, query),
        None => (&uri[7..], ""),
    };

    // Several addresses are allowed; one is enough to leave a list.
    let to = single_line(&percent_decode(to.split(',').next()?));
    let to = to.trim().to_string();
    if !is_plausible_address(&to) {
        return None;
    }

    let mut subject = None;
    let mut body = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        match key.to_ascii_lowercase().as_str() {
            "subject" => subject = Some(single_line(&percent_decode(value))),
            "body" => body = Some(percent_decode(value)),
            _ => {}
        }
    }

    let fallback = || "unsubscribe".to_string();
    Some(Method::Mail {
        to,
        subject: subject.filter(|s| !s.trim().is_empty()).unwrap_or_else(fallback),
        body: body.filter(|s| !s.trim().is_empty()).unwrap_or_else(fallback),
    })
}

/// Enough of an address to hand to SMTP: one `@`, something either side, and
/// nothing that could end a command line or an address.
fn is_plausible_address(addr: &str) -> bool {
    let mut parts = addr.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !addr.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
}

/// Takes out anything that would start a new header line.
///
/// A sender writes this URI, so a subject decoded out of it is untrusted text
/// on its way into a header.
pub(crate) fn single_line(value: &str) -> String {
    value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// `%XX` decoding. `+` stays a plus: in a `mailto:` it is not a space.
pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Some(byte) = std::str::from_utf8(hex).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The host part of a URL, for showing.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(header: &str, post: Option<&str>) -> ListUnsubscribe {
        ListUnsubscribe::from_headers(Some(header), post)
    }

    #[test]
    fn one_click_wins_when_the_sender_offers_it() {
        let list = offered(
            "<mailto:leave@lists.example>, <https://lists.example/u/123>",
            Some("List-Unsubscribe=One-Click"),
        );
        assert_eq!(list.method(), Some(Method::OneClick("https://lists.example/u/123".into())));
    }

    #[test]
    fn without_the_post_header_a_link_is_only_a_link() {
        let list = offered("<https://lists.example/u/123>, <mailto:leave@lists.example>", None);
        assert!(matches!(list.method(), Some(Method::Mail { .. })), "mail beats a web page");

        let list = offered("<https://lists.example/u/123>", None);
        assert_eq!(list.method(), Some(Method::Browser("https://lists.example/u/123".into())));
    }

    #[test]
    fn one_click_is_never_sent_over_plain_http() {
        let list = offered("<http://lists.example/u/1>", Some("List-Unsubscribe=One-Click"));
        assert_eq!(list.method(), Some(Method::Browser("http://lists.example/u/1".into())));
    }

    #[test]
    fn the_post_header_is_read_loosely() {
        let list = offered("<https://a.example/u>", Some(" list-unsubscribe = one-click "));
        assert!(matches!(list.method(), Some(Method::OneClick(_))));
    }

    #[test]
    fn a_mailto_carries_its_subject_and_body() {
        let list = offered("<mailto:leave@lists.example?subject=Remove%20me&body=bye>", None);
        assert_eq!(
            list.method(),
            Some(Method::Mail {
                to: "leave@lists.example".into(),
                subject: "Remove me".into(),
                body: "bye".into(),
            })
        );
    }

    #[test]
    fn a_bare_mailto_gets_a_subject_anyway() {
        let list = offered("<mailto:leave+abc@lists.example>", None);
        let Some(Method::Mail { to, subject, body }) = list.method() else { panic!() };
        assert_eq!(to, "leave+abc@lists.example", "a plus is part of the address");
        assert_eq!(subject, "unsubscribe");
        assert_eq!(body, "unsubscribe");
    }

    #[test]
    fn a_subject_cannot_smuggle_in_a_header() {
        let list = offered("<mailto:leave@lists.example?subject=hi%0D%0ABcc:%20x@evil.example>", None);
        let Some(Method::Mail { subject, .. }) = list.method() else { panic!() };
        assert!(!subject.contains('\n') && !subject.contains('\r'));
    }

    #[test]
    fn a_mailto_with_no_real_address_is_skipped() {
        for header in ["<mailto:>", "<mailto:nobody>", "<mailto:a@b@c.example>", "<mailto:a%0A@b.example>"] {
            assert_eq!(offered(header, None).method(), None, "{header}");
        }
    }

    #[test]
    fn a_folded_url_is_put_back_together() {
        let list = offered(
            "<https://lists.example/unsubscribe?\r\n token=abc>",
            Some("List-Unsubscribe=One-Click"),
        );
        assert_eq!(
            list.method(),
            Some(Method::OneClick("https://lists.example/unsubscribe?token=abc".into()))
        );
    }

    #[test]
    fn schemes_other_than_mail_and_web_are_ignored() {
        let list = offered("<javascript:alert(1)>, <ftp://x.example/>", Some("List-Unsubscribe=One-Click"));
        assert_eq!(list.method(), None);
    }

    #[test]
    fn no_header_is_a_known_answer() {
        assert_eq!(ListUnsubscribe::from_headers(None, None), ListUnsubscribe::Absent);
        assert_eq!(ListUnsubscribe::from_headers(Some("  "), None), ListUnsubscribe::Absent);
        assert_eq!(ListUnsubscribe::Unknown.method(), None);
    }

    #[test]
    fn the_target_names_who_is_being_asked() {
        assert_eq!(Method::OneClick("https://u:p@lists.example:443/x".into()).target(), "lists.example");
        let mail = Method::Mail { to: "a@b.example".into(), subject: String::new(), body: String::new() };
        assert_eq!(mail.target(), "a@b.example");
    }

    #[test]
    fn it_reads_the_headers_off_a_real_message() {
        let raw = b"From: news@shop.example\r\n\
            List-Unsubscribe: <mailto:u@shop.example>,\r\n <https://shop.example/u?id=1>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            Subject: Sale\r\n\r\nhello\r\n";
        let message = mail_parser::MessageParser::default().parse(&raw[..]).unwrap();
        let list = ListUnsubscribe::from_message(&message);
        assert_eq!(list.method(), Some(Method::OneClick("https://shop.example/u?id=1".into())));
    }
}
