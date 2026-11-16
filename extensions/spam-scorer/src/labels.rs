//! The labels: the kinds of message Jev is asked to tell apart, as whoever
//! installed this has set them up.
//!
//! A label is a name, a definition and whether it shows. The name and the
//! definition go to Jev as one option of the category question — the name is
//! what comes back, and the definition is what it is matched against — so
//! editing either changes what Jev is asked. Whether a label shows only
//! changes what is drawn: a message classified as a label that does not show
//! is marked as passed, like one that is none of them.

use serde::Deserialize;

/// The answer for a message that is none of the labels.
///
/// Always offered, and never drawn. Without it a message that fits nothing
/// would be forced into whichever label is least wrong, and "least wrong" is
/// not a thing anybody wants a badge for.
pub const OTHER: &str = "Other";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub definition: String,
    pub show: bool,
}

/// The labels, from the setting's JSON: one object per row, every value a
/// string. A row with no name is one that was added and never filled in.
pub fn parse(json: &str) -> Vec<Label> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(default)]
        name: String,
        #[serde(default)]
        definition: String,
        #[serde(default)]
        show: String,
    }

    let rows: Vec<Row> = serde_json::from_str(json).unwrap_or_default();
    let mut labels: Vec<Label> = Vec::new();
    for row in rows {
        let name = row.name.trim().to_string();
        let duplicate = labels.iter().any(|l| l.name.eq_ignore_ascii_case(&name));
        if name.is_empty() || duplicate || name.eq_ignore_ascii_case(OTHER) {
            continue;
        }
        labels.push(Label { name, definition: row.definition.trim().to_string(), show: row.show.trim() != "false" });
    }
    labels
}

/// What the labels ask Jev, as a short fingerprint.
///
/// Stored with every verdict. A verdict reached under different names or
/// definitions answered a different question, so it is scored again rather
/// than trusted — which is what makes editing a label change what the mail
/// already scored is called. Whether a label shows is not part of it: that
/// changes the drawing, not the question.
pub fn fingerprint(labels: &[Label]) -> String {
    // FNV-1a: stable across builds and platforms, which std's hasher is not
    // promised to be, and this is written down.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for label in labels {
        for byte in label.name.bytes().chain([0]).chain(label.definition.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// The label a category answer names, if it is one that shows.
pub fn shown<'a>(labels: &'a [Label], category: &str) -> Option<&'a Label> {
    labels.iter().find(|l| l.show && l.name == category)
}
