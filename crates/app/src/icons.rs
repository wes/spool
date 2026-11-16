//! The app's own icons, compiled into the binary.
//!
//! gpui reads icons through an [`AssetSource`], which is normally a directory
//! it loads from at runtime. This one is a table of `include_bytes!` instead:
//! an icon that is a file on disk is an icon that can go missing between the
//! build and the run, and a mail client that has to find its installation
//! directory before it can draw a paperclip has acquired a way to fail that it
//! did not need.
//!
//! The same reasoning as the app icon in `scripts/make_icon.py` — the artwork
//! is a source file, and what ships is derived from it.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Marks a message that has files hanging off it.
pub const PAPERCLIP: &str = "paperclip";

/// Somebody, as Clients knows them: the way into Clients from a message.
pub const PERSON: &str = "person";

/// The app's own icon, for the About window: a PNG drawn by
/// `scripts/make_icon.py`, the same script the bundle's icon comes from, so
/// the two cannot drift apart. Regenerate it with:
///
///     python3 scripts/make_icon.py /tmp/icon.png
///     sips -z 256 256 /tmp/icon.png --out crates/app/icons/spool.png
pub const APP_ICON: &str = "spool.png";
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/spool.png");

/// Every icon, by the name [`gpui::svg`] asks for.
const FILES: &[(&str, &[u8])] = &[
    (PAPERCLIP, include_bytes!("../icons/paperclip.svg")),
    (PERSON, include_bytes!("../icons/person.svg")),
];

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        // `None` rather than an error for an unknown name: gpui treats a
        // missing asset as nothing to draw, and an icon that fails to load
        // should cost a blank space rather than the frame it was in.
        if path == APP_ICON {
            return Ok(Some(Cow::Borrowed(APP_ICON_PNG)));
        }
        Ok(FILES
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .chain(std::iter::once(SharedString::from(APP_ICON)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_and_is_an_svg() {
        // The table is `include_bytes!`, so a missing file is a build error;
        // what this catches is a name that no longer matches its file, and
        // artwork that has been replaced by something gpui cannot rasterise.
        for (name, _) in FILES {
            let bytes = Icons.load(name).unwrap().expect(name);
            let text = std::str::from_utf8(&bytes).expect(name);
            assert!(text.contains("<svg"), "{name} is not an svg");
            assert!(text.contains("viewBox"), "{name} has no viewBox to scale by");
        }
    }

    #[test]
    fn the_app_icon_loads_and_is_a_png() {
        let bytes = Icons.load(APP_ICON).unwrap().expect("the app icon");
        assert!(bytes.starts_with(b"\x89PNG"), "not a PNG");
    }

    #[test]
    fn an_icon_that_is_not_there_is_not_an_error() {
        assert!(Icons.load("no-such-icon").unwrap().is_none());
    }
}
