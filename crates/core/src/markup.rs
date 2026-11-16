//! What someone typed, as the HTML of a message sent as HTML.
//!
//! The composer is a plain-text editor, and stays one: this is what lets it
//! send HTML without becoming a word processor. What is typed is read the way
//! mail has always been written — blank lines between paragraphs, `- ` for a
//! list, `> ` for a quote — plus the three marks everyone already types by
//! hand: `**bold**`, `*italic*` and a bare link. Everything else is text, and
//! is escaped as text: a `<` typed in a message is a less-than sign, not a tag.

/// A plain-text body as simple HTML: paragraphs, line breaks, lists, quotes,
/// links and emphasis. The result is a fragment, for a document to wrap.
pub fn to_html(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    let mut ix = 0;

    while ix < lines.len() {
        let line = lines[ix];
        if line.trim().is_empty() {
            ix += 1;
            continue;
        }

        if quoted(line).is_some() {
            let mut inner = Vec::new();
            while ix < lines.len() && let Some(rest) = quoted(lines[ix]) {
                inner.push(rest);
                ix += 1;
            }
            out.push_str(&format!("<blockquote {QUOTE_STYLE}>{}</blockquote>\n", to_html(&inner.join("\n"))));
            continue;
        }

        if let Some((ordered, _)) = item(line) {
            let tag = if ordered { "ol" } else { "ul" };
            out.push_str(&format!("<{tag}>\n"));
            while ix < lines.len() && let Some((_, rest)) = item(lines[ix]).filter(|(o, _)| *o == ordered) {
                out.push_str(&format!("<li>{}</li>\n", inline(rest)));
                ix += 1;
            }
            out.push_str(&format!("</{tag}>\n"));
            continue;
        }

        // A paragraph: every line up to a blank one, or to one that starts a
        // list or a quote, with the line breaks as they were typed.
        let mut paragraph = Vec::new();
        while ix < lines.len() {
            let line = lines[ix];
            if line.trim().is_empty() || quoted(line).is_some() || item(line).is_some() {
                break;
            }
            paragraph.push(inline(line));
            ix += 1;
        }
        out.push_str(&format!("<p style=\"margin:0 0 1em\">{}</p>\n", paragraph.join("<br>\n")));
    }
    out
}

/// How a quote is drawn: the rule down the left that every client uses.
pub const QUOTE_STYLE: &str =
    "type=\"cite\" style=\"margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex\"";

/// Text as HTML text: the five characters that mean something escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The inside of a document's `<body>`, or all of it when it has none.
///
/// A message's HTML is usually a whole document, and one document cannot sit
/// inside another: its `<head>` — above all its stylesheet — would apply to
/// the reply around it. What is quoted is what was on the page.
pub fn body_of(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<body") else { return html };
    let Some(start) = lower[open..].find('>').map(|end| open + end + 1) else { return html };
    let end = lower[start..].find("</body").map_or(html.len(), |end| start + end);
    &html[start..end]
}

/// An HTML body as plain text, for the text version sent beside it.
///
/// Read the way the reader reads HTML (see [`crate::html`]), then written out
/// the way mail is written: blank lines between paragraphs, `> ` in front of
/// quoted lines, `- ` or a number in front of list items.
pub fn to_text(html: &str) -> String {
    use crate::html::Block;

    let mut out = String::new();
    let mut last_was_item = false;
    for block in crate::html::html_to_blocks(html) {
        let text = block.text();
        let (lines, item) = match &block {
            Block::Quote { depth, .. } => {
                let marker = "> ".repeat(usize::from((*depth).max(1)));
                let lines: Vec<String> = text
                    .lines()
                    .map(|line| format!("{marker}{line}").trim_end().to_string())
                    .collect();
                (lines.join("\n"), false)
            }
            Block::ListItem { depth, ordered, index, .. } => {
                let indent = "  ".repeat(usize::from(depth.saturating_sub(1)));
                let marker = if *ordered { format!("{index}. ") } else { "- ".to_string() };
                (format!("{indent}{marker}{}", text.trim()), true)
            }
            Block::Rule => ("---".to_string(), false),
            _ => (text.trim_end().to_string(), false),
        };
        if lines.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            // List items sit on consecutive lines; everything else is a
            // paragraph of its own.
            out.push_str(if item && last_was_item { "\n" } else { "\n\n" });
        }
        out.push_str(&lines);
        last_was_item = item;
    }
    out
}

/// The attribute a remote image's address is parked in while it is being
/// edited. See [`hold_remote`].
const HELD: &str = "data-spool-";

/// Stops remote images loading while a message is being edited.
///
/// The editor shows the original a reply quotes, and loading its images would
/// tell the sender — again — that their mail was read. So each remote `src`
/// and `srcset` is renamed to one the web view ignores, and renamed back by
/// [`release_remote`] before anything is sent: what goes out is exactly what
/// came in.
pub fn hold_remote(html: &str) -> String {
    rename_remote(html, &["src", "srcset"], |name| format!("{HELD}{name}"))
}

/// Undoes [`hold_remote`].
pub fn release_remote(html: &str) -> String {
    let mut out = html.to_string();
    for name in ["srcset", "src"] {
        out = out.replace(&format!(" {HELD}{name}="), &format!(" {name}="));
    }
    out
}

/// Renames the attributes in `names` whose value is an `http(s):` URL.
fn rename_remote(html: &str, names: &[&str], rename: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while at < html.len() {
        let next = names
            .iter()
            .filter_map(|name| {
                let needle = format!("{name}=");
                lower[at..].find(&needle).map(|ix| (at + ix, *name))
            })
            .min_by_key(|(ix, _)| *ix);
        let Some((ix, name)) = next else {
            out.push_str(&html[at..]);
            break;
        };
        let value = &lower[ix + name.len() + 1..];
        let value = value.trim_start_matches(['"', '\'']);
        let whole_word = ix > 0 && lower.as_bytes()[ix - 1].is_ascii_whitespace();
        out.push_str(&html[at..ix]);
        if whole_word && (value.starts_with("http://") || value.starts_with("https://")) {
            out.push_str(&rename(name));
        } else {
            out.push_str(&html[ix..ix + name.len()]);
        }
        at = ix + name.len();
    }
    out
}

/// A line of a `> ` quote, and what follows the marker.
fn quoted(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// A list item: whether it is numbered, and its text.
fn item(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    for bullet in ["- ", "* ", "\u{2022} "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return Some((false, rest));
        }
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &trimmed[digits..];
    (digits > 0 && digits <= 3)
        .then(|| rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")))
        .flatten()
        .map(|rest| (true, rest))
}

/// One line's text: escaped, with its links and emphasis marked up.
fn inline(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        // The earliest link starts a run that is not emphasised: a `*` in a
        // URL is part of the URL.
        match find_link(rest) {
            Some((start, end)) => {
                out.push_str(&emphasis(&escape(&rest[..start])));
                let url = &rest[start..end];
                out.push_str(&format!("<a href=\"{}\">{}</a>", escape(url), escape(url)));
                rest = &rest[end..];
            }
            None => {
                out.push_str(&emphasis(&escape(rest)));
                break;
            }
        }
    }
    out
}

/// Where the first `http://` or `https://` link in a line starts and ends.
///
/// A sentence's closing punctuation is left out, so "see https://x.com." links
/// to `x.com` rather than to `x.com.`.
fn find_link(text: &str) -> Option<(usize, usize)> {
    let start = ["https://", "http://"].iter().filter_map(|p| text.find(p)).min()?;
    let len = text[start..].find(char::is_whitespace).unwrap_or(text.len() - start);
    let url = text[start..start + len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']);
    (url.len() > "https://".len()).then_some((start, start + url.len()))
}

/// `**bold**` and `*italic*` (or `_italic_`) in text already escaped.
///
/// A mark counts only when it hugs a word on both sides — `2 * 3 * 4` and a
/// lone asterisk are left as they are.
fn emphasis(text: &str) -> String {
    let text = wrap(text, "**", "strong");
    let text = wrap(&text, "*", "em");
    wrap(&text, "_", "em")
}

fn wrap(text: &str, mark: &str, tag: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(mark) {
        let after = &rest[open + mark.len()..];
        let hugs_open = after.chars().next().is_some_and(|c| !c.is_whitespace());
        let before_ok = rest[..open].chars().last().is_none_or(|c| !c.is_alphanumeric());
        let close = after.find(mark).filter(|&close| {
            close > 0 && after[..close].chars().last().is_some_and(|c| !c.is_whitespace())
        });
        match (hugs_open && before_ok, close) {
            (true, Some(close)) => {
                out.push_str(&rest[..open]);
                out.push_str(&format!("<{tag}>{}</{tag}>", &after[..close]));
                rest = &after[close + mark.len()..];
            }
            _ => {
                out.push_str(&rest[..open + mark.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_and_line_breaks_are_kept() {
        assert_eq!(
            to_html("Hi Ana,\n\nThanks for this.\nSee you then."),
            "<p style=\"margin:0 0 1em\">Hi Ana,</p>\n\
             <p style=\"margin:0 0 1em\">Thanks for this.<br>\nSee you then.</p>\n"
        );
    }

    #[test]
    fn what_is_typed_is_text_not_markup() {
        let html = to_html("a <b>tag</b> & \"quotes\"");
        assert!(html.contains("a &lt;b&gt;tag&lt;/b&gt; &amp; &quot;quotes&quot;"), "{html}");
    }

    #[test]
    fn lists_become_lists() {
        let html = to_html("Two things:\n- one\n- two\n\n1. first\n2. second");
        assert!(html.contains("<ul>\n<li>one</li>\n<li>two</li>\n</ul>"), "{html}");
        assert!(html.contains("<ol>\n<li>first</li>\n<li>second</li>\n</ol>"), "{html}");
        assert!(html.contains("Two things:</p>"), "{html}");
    }

    #[test]
    fn a_quote_is_a_blockquote_and_can_hold_its_own() {
        let html = to_html("> hello\n> > older");
        assert_eq!(html.matches("<blockquote").count(), 2, "{html}");
        assert!(html.contains("hello"), "{html}");
    }

    #[test]
    fn links_and_emphasis_are_marked() {
        let html = to_html("See https://example.com/a*b. It is **very** *good*, 2 * 3 is _six_.");
        assert!(html.contains("<a href=\"https://example.com/a*b\">https://example.com/a*b</a>."), "{html}");
        assert!(html.contains("<strong>very</strong>"), "{html}");
        assert!(html.contains("<em>good</em>"), "{html}");
        assert!(html.contains("2 * 3"), "a lone asterisk is left alone: {html}");
        assert!(html.contains("<em>six</em>"), "{html}");
    }

    #[test]
    fn a_link_with_a_quote_in_it_cannot_break_out_of_its_attribute() {
        let html = to_html("https://x.com/\"onmouseover=\"alert(1)");
        assert!(!html.contains("\"onmouseover"), "{html}");
    }

    #[test]
    fn html_reads_back_as_mail_text() {
        let text = to_html("Hi Ana,\n\nTwo things:\n- one\n- two\n\n> Are we on?");
        assert_eq!(super::to_text(&text), "Hi Ana,\n\nTwo things:\n\n- one\n- two\n\n> Are we on?");
    }

    #[test]
    fn remote_images_are_held_while_editing_and_sent_as_they_came() {
        let html = r#"<img src="https://x.com/a.png"><img SRC='http://y.com/b.gif'><img src="data:image/png;base64,AA"><p>src=http not an attribute</p>"#;
        let held = hold_remote(html);
        assert!(!held.contains(" src=\"https://"), "{held}");
        assert!(!held.to_lowercase().contains(" src='http://"), "{held}");
        assert!(held.contains(r#"src="data:image/png"#), "an embedded image is not remote: {held}");
        assert!(held.contains("<p>src=http not an attribute</p>"), "text is not an attribute: {held}");
        assert_eq!(release_remote(&held).to_lowercase(), html.to_lowercase());
    }

    #[test]
    fn a_documents_body_is_what_is_quoted() {
        let doc = "<html><head><style>p{color:red}</style></head><BODY class=x><p>Hi</p></body></html>";
        assert_eq!(body_of(doc), "<p>Hi</p>");
        assert_eq!(body_of("<p>fragment</p>"), "<p>fragment</p>");
    }
}
