//! `mailto:` links (RFC 6068), read as a message to start writing.
//!
//! What a browser, a PDF or another app hands over when somebody clicks an
//! email address and Spool is the app that opens them. Everything in it was
//! written by whoever made the page, so it is read the way a header from a
//! stranger is read: nothing in it may start a second header line.

use crate::address::{Address, decode_list};
use crate::unsubscribe::{percent_decode, single_line};

/// The message a `mailto:` link asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mailto {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body: String,
}

impl Mailto {
    /// Reads a link, or `None` when it is not a `mailto:` link at all.
    ///
    /// A `mailto:` with nothing after it is still a link: it asks for a blank
    /// message, which is a reasonable thing for a "contact us" button to do.
    pub fn parse(uri: &str) -> Option<Self> {
        let uri = uri.trim();
        let rest = uri
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
            .map(|_| &uri[7..])?;
        // A fragment means nothing to a message, and left in place it would
        // end up on the last address or in the body.
        let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));

        let mut mail = Mailto { to: addresses(path), ..Mailto::default() };
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match percent_decode(key).to_ascii_lowercase().as_str() {
                // RFC 6068 lets `to` appear in the query as well as before it,
                // and both count.
                "to" => mail.to.extend(addresses(value)),
                "cc" => mail.cc.extend(addresses(value)),
                "bcc" => mail.bcc.extend(addresses(value)),
                "subject" => mail.subject = single_line(&percent_decode(value)).trim().to_string(),
                // The body is the one part allowed more than one line. The RFC
                // spells a line break `%0D%0A`; the composer writes `\n`.
                "body" => mail.body = percent_decode(value).replace("\r\n", "\n").replace('\r', "\n"),
                // `In-Reply-To` and the rest are allowed by the RFC and ignored
                // here: a web page has no business threading a message into a
                // conversation it cannot see.
                _ => {}
            }
        }
        Some(mail)
    }
}

/// The addresses in one part of a link, decoded.
///
/// What does not look like an address is dropped rather than kept as text: a
/// recipient field holding `%20` or half a domain is something the person has
/// to notice and delete before they can send.
fn addresses(encoded: &str) -> Vec<Address> {
    let decoded = single_line(&percent_decode(encoded));
    decode_list(&decoded)
        .into_iter()
        .map(|a| Address::new(a.name.trim(), a.addr.trim()))
        .filter(|a| is_plausible(&a.addr))
        .collect()
}

fn is_plausible(addr: &str) -> bool {
    let mut parts = addr.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !addr.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ','))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[Address]) -> Vec<&str> {
        list.iter().map(|a| a.addr.as_str()).collect()
    }

    #[test]
    fn a_bare_address_is_the_recipient() {
        let mail = Mailto::parse("mailto:ana@example.com").unwrap();
        assert_eq!(addrs(&mail.to), ["ana@example.com"]);
        assert!(mail.subject.is_empty() && mail.body.is_empty());
    }

    #[test]
    fn the_scheme_is_not_case_sensitive() {
        assert!(Mailto::parse("MAILTO:ana@example.com").is_some());
        assert!(Mailto::parse("https://example.com").is_none());
        assert!(Mailto::parse("mail").is_none());
    }

    #[test]
    fn subject_body_and_copies_are_read() {
        let mail = Mailto::parse(
            "mailto:ana@example.com,bo@example.com?cc=cy@example.com&bcc=di@example.com\
             &subject=Hello%20there&body=Line%20one%0D%0ALine%20two",
        )
        .unwrap();
        assert_eq!(addrs(&mail.to), ["ana@example.com", "bo@example.com"]);
        assert_eq!(addrs(&mail.cc), ["cy@example.com"]);
        assert_eq!(addrs(&mail.bcc), ["di@example.com"]);
        assert_eq!(mail.subject, "Hello there");
        assert_eq!(mail.body, "Line one\nLine two");
    }

    #[test]
    fn to_in_the_query_adds_to_the_path() {
        let mail = Mailto::parse("mailto:ana@example.com?to=bo@example.com").unwrap();
        assert_eq!(addrs(&mail.to), ["ana@example.com", "bo@example.com"]);

        let mail = Mailto::parse("mailto:?to=bo@example.com&subject=hi").unwrap();
        assert_eq!(addrs(&mail.to), ["bo@example.com"]);
    }

    #[test]
    fn an_empty_link_is_a_blank_message() {
        assert_eq!(Mailto::parse("mailto:"), Some(Mailto::default()));
    }

    #[test]
    fn encoded_addresses_and_names_are_decoded() {
        let mail = Mailto::parse("mailto:%22Ana%20Ruiz%22%20%3Cana%40example.com%3E").unwrap();
        assert_eq!(mail.to, [Address::new("Ana Ruiz", "ana@example.com")]);
    }

    #[test]
    fn a_plus_stays_a_plus() {
        let mail = Mailto::parse("mailto:ana+news@example.com?subject=1+1").unwrap();
        assert_eq!(addrs(&mail.to), ["ana+news@example.com"]);
        assert_eq!(mail.subject, "1+1");
    }

    #[test]
    fn a_subject_cannot_add_a_header() {
        let mail = Mailto::parse("mailto:ana@example.com?subject=hi%0D%0ABcc:%20x@evil.example").unwrap();
        assert!(!mail.subject.contains(['\r', '\n']), "{:?}", mail.subject);
        assert!(mail.bcc.is_empty());
    }

    #[test]
    fn an_address_cannot_smuggle_in_a_line_break() {
        let mail = Mailto::parse("mailto:ana@example.com%0D%0ABcc:x@evil.example").unwrap();
        for a in &mail.to {
            assert!(!a.addr.contains(['\r', '\n']), "{a:?}");
        }
    }

    #[test]
    fn what_is_not_an_address_is_dropped() {
        let mail = Mailto::parse("mailto:nobody,ana@example.com,%20,a@b@c").unwrap();
        assert_eq!(addrs(&mail.to), ["ana@example.com"]);
    }

    #[test]
    fn a_fragment_is_ignored() {
        let mail = Mailto::parse("mailto:ana@example.com?subject=hi#top").unwrap();
        assert_eq!(mail.subject, "hi");
    }
}
