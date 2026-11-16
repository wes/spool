//! Decoding and gating the images in a message.
//!
//! Two kinds, and the difference is the whole design. An image that **arrived
//! with the message** costs nothing to show: the bytes are already on disk, so
//! it is decoded when the message opens and drawn like any other content. An
//! image the message **points at** costs a request to a server the sender
//! chose, and that request is itself the message: a unique URL per recipient
//! is how open-tracking works, which is why remote images are blocked until
//! someone asks for them.
//!
//! Nothing here fetches. The store holds what has been decoded and what the
//! reader has consented to; the workspace starts the requests, and only in
//! response to a click.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{Image, ImageFormat};
use spool_core::{Body, MessageId, html};

/// The largest remote image worth pulling down.
///
/// Generous for a banner, small enough that a hostile or broken server cannot
/// spend the reader's memory.
pub const MAX_REMOTE_BYTES: usize = 8 * 1024 * 1024;

/// What has happened to one remote image.
#[derive(Debug, Clone)]
pub enum Remote {
    Loading,
    Ready(Arc<Image>),
    /// Kept as a sentence, because "this image did not load" with no reason is
    /// the most annoying possible outcome.
    Failed(String),
}

/// Per-session image state.
#[derive(Default)]
pub struct ImageStore {
    /// Whether the reader has said yes to all of it, once, in Settings.
    ///
    /// Kept beside the per-message set rather than replacing it: the setting
    /// is a standing answer and the set is a series of one-off ones, and
    /// turning the setting back off must not silently un-answer the messages
    /// already loaded in this session.
    always: bool,
    /// Messages whose remote images the reader has agreed to load. Consent is
    /// per message and never inferred: opening one message says nothing about
    /// the next.
    allowed: HashSet<MessageId>,
    /// Sender domains the reader has said yes to for good, lowercased.
    ///
    /// The middle ground the two answers above leave out: a newsletter you
    /// read for the pictures should not need the button every week, and
    /// saying so about one sender is a much smaller decision than saying it
    /// about every sender there will ever be. Kept in the store beside the
    /// rest so that every place asking "may I draw this" asks one question,
    /// and mirrored into the settings file so the answer outlives the
    /// session.
    domains: HashSet<String>,
    /// Fetched remote images, keyed by URL and shared across messages — a
    /// newsletter's logo is the same bytes in every issue.
    remote: HashMap<String, Remote>,
}

impl ImageStore {
    /// Whether this message's remote images may be drawn.
    ///
    /// `domain` is the sender's, which is what makes a standing answer about
    /// one sender apply to mail they have not sent yet.
    pub fn is_allowed(&self, message: MessageId, domain: Option<&str>) -> bool {
        self.always || self.allowed.contains(&message) || self.trusts(domain)
    }

    /// Whether images from this sender load without being asked for.
    pub fn trusts(&self, domain: Option<&str>) -> bool {
        domain.is_some_and(|domain| self.domains.contains(domain))
    }

    /// Replaces the trusted domains with what the settings file holds.
    pub fn set_domains(&mut self, domains: impl IntoIterator<Item = String>) {
        self.domains = domains.into_iter().map(|d| d.trim().to_ascii_lowercase()).collect();
    }

    /// Says yes to this sender from now on.
    pub fn trust_domain(&mut self, domain: &str) {
        self.domains.insert(domain.trim().to_ascii_lowercase());
    }

    /// Takes it back. What was already fetched stays in the cache — the next
    /// message from them simply arrives blocked again.
    pub fn forget_domain(&mut self, domain: &str) {
        self.domains.remove(&domain.trim().to_ascii_lowercase());
    }

    /// The trusted senders, in a stable order for the settings pane.
    pub fn domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.domains.iter().cloned().collect();
        domains.sort();
        domains
    }

    /// Withdraws the one-off consent given to a single message.
    ///
    /// What makes "stop loading images from this sender" visible: without it
    /// the message on screen keeps its images, having been allowed in its own
    /// right a moment ago, and the click appears to have done nothing.
    pub fn disallow(&mut self, message: MessageId) {
        self.allowed.remove(&message);
    }

    /// Sets the standing answer from the preference.
    ///
    /// Returns whether it changed, because turning it on has to be followed
    /// by actually fetching what the open message needs — the reader can now
    /// show remote images, but nothing has asked for them yet.
    pub fn set_always(&mut self, always: bool) -> bool {
        let changed = self.always != always;
        self.always = always;
        changed
    }

    pub fn always(&self) -> bool {
        self.always
    }

    pub fn allow(&mut self, message: MessageId) {
        self.allowed.insert(message);
    }

    pub fn remote(&self, url: &str) -> Option<&Remote> {
        self.remote.get(url)
    }

    /// Claims a URL for fetching. `false` means someone already has it, which
    /// is what stops four copies of the same logo becoming four requests.
    pub fn begin(&mut self, url: &str) -> bool {
        if self.remote.contains_key(url) {
            return false;
        }
        self.remote.insert(url.to_string(), Remote::Loading);
        true
    }

    pub fn finish(&mut self, url: String, outcome: Remote) {
        self.remote.insert(url, outcome);
    }
}

/// The images one open message can draw without asking anyone.
///
/// Built once when the message opens, so painting a frame never decodes a PNG.
#[derive(Default)]
pub struct Embedded(HashMap<String, Arc<Image>>);

impl Embedded {
    /// Decodes every `cid:` part the body carries and every `data:` URL its
    /// HTML contains.
    pub fn build(body: &Body, blocks: &[html::Block]) -> Self {
        let mut map = HashMap::new();

        for image in &body.inline_images {
            if let Some(decoded) = decode(&image.mime, image.bytes.clone()) {
                map.insert(spool_core::normalize_content_id(&image.content_id), decoded);
            }
        }

        for block in blocks {
            let html::Block::Image(image) = block else { continue };
            let html::ImageSource::Data { mime, base64 } = &image.source else { continue };
            if map.contains_key(base64.as_str()) {
                continue;
            }
            // The only base64 in the app crate, and it belongs to the net
            // crate's encoder — one implementation, already tested.
            if let Some(bytes) = spool_net::encoding::base64_decode(base64)
                && let Some(decoded) = decode(mime, bytes)
            {
                map.insert(base64.clone(), decoded);
            }
        }

        Self(map)
    }

    pub fn get(&self, key: &str) -> Option<&Arc<Image>> {
        self.0.get(key)
    }

    /// The key an image block looks itself up by.
    pub fn key(source: &html::ImageSource) -> Option<String> {
        match source {
            html::ImageSource::Cid(id) => Some(spool_core::normalize_content_id(id)),
            html::ImageSource::Data { base64, .. } => Some(base64.clone()),
            html::ImageSource::Remote(_) => None,
        }
    }
}

/// Wraps bytes for gpui, choosing a format from the declared type and, failing
/// that, from the bytes themselves.
///
/// Senders label images wrongly often enough that sniffing is not paranoia —
/// `image/jpg`, `application/octet-stream` and a plain `image` all turn up.
pub fn decode(mime: &str, bytes: Vec<u8>) -> Option<Arc<Image>> {
    if bytes.is_empty() {
        return None;
    }
    let format = format_from_mime(mime).or_else(|| sniff(&bytes))?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn format_from_mime(mime: &str) -> Option<ImageFormat> {
    match mime.trim().to_ascii_lowercase().split(';').next()?.trim() {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        "image/bmp" | "image/x-ms-bmp" => Some(ImageFormat::Bmp),
        // SVG is deliberately absent: it is a document format with scripting
        // and remote references, and rendering one from a stranger is a
        // different decision than rendering a bitmap.
        _ => None,
    }
}

/// Identifies a format from its magic bytes.
fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

    if bytes.starts_with(PNG) {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if bytes.starts_with(b"BM") {
        return Some(ImageFormat::Bmp);
    }
    // RIFF....WEBP
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    None
}

/// Every remote URL a set of blocks would like to load, in document order and
/// without repeats.
pub fn remote_urls(blocks: &[html::Block]) -> Vec<String> {
    let mut urls = Vec::new();
    for block in blocks {
        if let html::Block::Image(image) = block
            && let html::ImageSource::Remote(url) = &image.source
            && !urls.iter().any(|seen| seen == url)
        {
            urls.push(url.clone());
        }
    }
    urls
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_preference_answers_for_every_message_at_once() {
        let mut store = ImageStore::default();
        assert!(!store.is_allowed(MessageId(1), None), "blocked is the default");

        assert!(store.set_always(true), "and the caller is told it changed");
        assert!(store.is_allowed(MessageId(1), None));
        assert!(store.is_allowed(MessageId(2), None), "including messages never opened");

        assert!(!store.set_always(true), "setting it to what it already is changes nothing");
    }

    #[test]
    fn turning_the_preference_off_does_not_take_back_a_message_already_loaded() {
        // The images are already on screen and already fetched — the sender
        // has already been told. Hiding them again would cost the privacy
        // nothing and only make the reader flicker.
        let mut store = ImageStore::default();
        store.allow(MessageId(7));
        store.set_always(true);
        store.set_always(false);

        assert!(store.is_allowed(MessageId(7), None), "this one was said yes to by hand");
        assert!(!store.is_allowed(MessageId(8), None), "and the rest go back to blocked");
    }

    #[test]
    fn a_trusted_sender_is_answered_for_before_the_message_is_opened() {
        let mut store = ImageStore::default();
        store.trust_domain("news.temu.com");

        assert!(store.is_allowed(MessageId(1), Some("news.temu.com")));
        assert!(
            !store.is_allowed(MessageId(1), Some("temu.com")),
            "a different host is a different sender",
        );
        assert!(!store.is_allowed(MessageId(1), None), "and mail from nowhere stays blocked");
    }

    #[test]
    fn a_sender_is_trusted_however_their_address_was_capitalised() {
        let mut store = ImageStore::default();
        store.trust_domain("  News.Temu.COM ");
        assert!(store.is_allowed(MessageId(1), Some("news.temu.com")));

        store.forget_domain("NEWS.TEMU.COM");
        assert!(!store.is_allowed(MessageId(1), Some("news.temu.com")));
    }

    #[test]
    fn backing_out_of_a_sender_takes_the_open_message_with_it() {
        // Otherwise the message on screen keeps its images — it was allowed
        // in its own right when the domain rule loaded it — and the click
        // looks like it did nothing.
        let mut store = ImageStore::default();
        store.trust_domain("news.temu.com");
        store.allow(MessageId(3));

        store.forget_domain("news.temu.com");
        assert!(store.is_allowed(MessageId(3), Some("news.temu.com")), "still allowed by hand");

        store.disallow(MessageId(3));
        assert!(!store.is_allowed(MessageId(3), Some("news.temu.com")));
    }

    #[test]
    fn the_trusted_senders_come_back_in_a_settled_order() {
        let mut store = ImageStore::default();
        store.set_domains(["Zeta.example".to_string(), "alpha.example".to_string()]);
        assert_eq!(store.domains(), vec!["alpha.example", "zeta.example"]);
    }

    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0];

    #[test]
    fn a_declared_type_picks_the_format() {
        assert!(decode("image/png", PNG.to_vec()).is_some());
        assert!(decode("image/jpeg", vec![1, 2, 3]).is_some());
        // Parameters and case are both things senders vary.
        assert!(decode("IMAGE/PNG; name=logo.png", PNG.to_vec()).is_some());
    }

    #[test]
    fn a_wrong_or_missing_type_falls_back_to_the_bytes() {
        // `application/octet-stream` on a PNG is common enough to matter.
        assert!(decode("application/octet-stream", PNG.to_vec()).is_some());
        assert!(decode("", vec![0xff, 0xd8, 0xff, 0]).is_some(), "JPEG magic");
        assert!(decode("", b"GIF89a...".to_vec()).is_some());

        // Nothing to go on: better to show the alt text than to hand gpui
        // bytes it will fail to decode.
        assert!(decode("text/html", b"<html>".to_vec()).is_none());
        assert!(decode("image/png", Vec::new()).is_none(), "empty is not an image");
    }

    #[test]
    fn svg_is_not_treated_as_an_image() {
        // A document format with scripting and remote references; rendering
        // one from a stranger is a different decision than a bitmap.
        assert!(decode("image/svg+xml", b"<svg/>".to_vec()).is_none());
    }

    #[test]
    fn consent_is_per_message_and_never_inferred() {
        let mut store = ImageStore::default();
        assert!(!store.is_allowed(MessageId(1), None));

        store.allow(MessageId(1));
        assert!(store.is_allowed(MessageId(1), None));
        assert!(!store.is_allowed(MessageId(2), None), "one message says nothing about another");
    }

    #[test]
    fn a_url_is_only_fetched_once() {
        let mut store = ImageStore::default();
        assert!(store.begin("https://example.com/a.png"));
        assert!(!store.begin("https://example.com/a.png"), "already in flight");

        store.finish("https://example.com/a.png".into(), Remote::Failed("nope".into()));
        assert!(!store.begin("https://example.com/a.png"), "and not retried on its own");
        assert!(matches!(store.remote("https://example.com/a.png"), Some(Remote::Failed(_))));
    }

    #[test]
    fn remote_urls_are_collected_once_each_in_order() {
        let blocks = html::html_to_blocks(
            r#"<img src="https://a.example/1.png" width="10">
               <img src="cid:local" width="10">
               <img src="https://b.example/2.png" width="10">
               <img src="https://a.example/1.png" width="10">"#,
        );
        assert_eq!(
            remote_urls(&blocks),
            ["https://a.example/1.png", "https://b.example/2.png"],
            "cid images are not remote, and a repeat is not a second request"
        );
    }

    #[test]
    fn embedded_images_are_keyed_the_way_blocks_look_them_up() {
        let cid = html::ImageSource::Cid("<Logo@Example.com>".into());
        assert_eq!(Embedded::key(&cid).as_deref(), Some("logo@example.com"));

        let data = html::ImageSource::Data { mime: "image/png".into(), base64: "AAA".into() };
        assert_eq!(Embedded::key(&data).as_deref(), Some("AAA"));

        assert_eq!(Embedded::key(&html::ImageSource::Remote("https://x/y".into())), None);
    }

    #[test]
    fn an_attached_image_is_decoded_when_the_message_opens() {
        let body = Body {
            html: r#"<img src="cid:logo@example.com" width="80">"#.into(),
            inline_images: vec![spool_core::InlineImage {
                content_id: "logo@example.com".into(),
                mime: "image/png".into(),
                bytes: PNG.to_vec(),
            }],
            ..Default::default()
        };
        let blocks = html::html_to_blocks(&body.html);
        let embedded = Embedded::build(&body, &blocks);

        let html::Block::Image(image) = &blocks[0] else { panic!("expected an image block") };
        let key = Embedded::key(&image.source).unwrap();
        assert!(embedded.get(&key).is_some());
    }
}
