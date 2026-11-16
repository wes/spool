//! What the text widgets share about moving through text and taking edits
//! back: where ⌥← and ⌥→ land, and the undo history behind ⌘Z.
//!
//! Kept apart from the widgets because none of it needs a window, which is
//! what makes it testable, and because a field and an area that disagreed
//! about where a word ends would be worse than either being wrong alone.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// Whether a UAX #29 segment is a word, as opposed to the space or
/// punctuation between words — which ⌥-movement skips over.
fn is_word(segment: &str) -> bool {
    segment.chars().any(char::is_alphanumeric)
}

/// Where ⌥← lands: the start of the word the caret is in, or of the one
/// before it when the caret is already at a word's start or between words.
pub fn previous_word_start(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text.split_word_bound_indices()
        .take_while(|(start, _)| *start < offset)
        .filter(|(_, segment)| is_word(segment))
        .last()
        .map(|(start, _)| start)
        .unwrap_or(0)
}

/// Where ⌥→ lands: the end of the word the caret is in, or of the next one.
pub fn next_word_end(text: &str, offset: usize) -> usize {
    text.split_word_bound_indices()
        .map(|(start, segment)| (start, start + segment.len(), segment))
        .find(|(_, end, segment)| *end > offset && is_word(segment))
        .map(|(_, end, _)| end)
        .unwrap_or(text.len())
}

/// Where ⌥↑ lands: the start of the paragraph, or of the one before when the
/// caret is already there.
pub fn previous_paragraph_start(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let search = offset.saturating_sub(1);
    text[..search].rfind('\n').map(|ix| ix + 1).unwrap_or(0)
}

/// Where ⌥↓ lands: the end of the paragraph, or of the next one when the
/// caret is already there.
pub fn next_paragraph_end(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let from = match text[offset..].starts_with('\n') {
        true => offset + 1,
        false => offset,
    };
    text[from..].find('\n').map(|ix| from + ix).unwrap_or(text.len())
}

/// The kind of edit, which decides whether it joins the step before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typing, a letter at a time: a run of it is undone as one step, the
    /// way it is in every Mac text field.
    Typing,
    /// Backspace or forward delete, a letter at a time — the same, for a run
    /// of deleting.
    Deleting,
    /// Anything else — a paste, a cut, a word deleted at once — which is a
    /// step of its own.
    Other,
}

#[derive(Debug, Clone)]
struct Snapshot {
    content: String,
    selection: Range<usize>,
}

/// The undo and redo stacks for one piece of text.
///
/// Whole snapshots rather than diffs. The text is an email, not a codebase,
/// and a snapshot can never be applied to the wrong text.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The kind of the last edit and where the caret was left after it, so
    /// the next one can tell whether it carries on from it.
    last: Option<(EditKind, usize)>,
}

/// More than anybody will press ⌘Z for, and a bound on what a very long
/// session of writing can hold on to.
const LIMIT: usize = 500;

impl History {
    /// Called before an edit is made to `content`. `cursor` is where the edit
    /// starts from, so a run of typing that the caret left part-way through
    /// is not joined to the next.
    pub fn record(&mut self, kind: EditKind, content: &str, selection: Range<usize>, cursor: usize) {
        let continues = kind != EditKind::Other
            && selection.is_empty()
            && self.last == Some((kind, cursor));
        if !continues {
            self.undo.push(Snapshot { content: content.to_string(), selection });
            if self.undo.len() > LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    /// Called after the edit, with where it left the caret.
    pub fn edited(&mut self, kind: EditKind, cursor: usize) {
        self.last = Some((kind, cursor));
    }

    /// The state to go back to, given the current one to keep for redo.
    pub fn undo(&mut self, content: &str, selection: Range<usize>) -> Option<(String, Range<usize>)> {
        let snapshot = self.undo.pop()?;
        self.redo.push(Snapshot { content: content.to_string(), selection });
        self.last = None;
        Some((snapshot.content, snapshot.selection))
    }

    pub fn redo(&mut self, content: &str, selection: Range<usize>) -> Option<(String, Range<usize>)> {
        let snapshot = self.redo.pop()?;
        self.undo.push(Snapshot { content: content.to_string(), selection });
        self.last = None;
        Some((snapshot.content, snapshot.selection))
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_left_goes_to_the_start_of_the_word_or_the_one_before() {
        let text = "hello, big world";
        assert_eq!(previous_word_start(text, 16), 11);
        assert_eq!(previous_word_start(text, 13), 11);
        assert_eq!(previous_word_start(text, 11), 7);
        assert_eq!(previous_word_start(text, 7), 0);
        assert_eq!(previous_word_start(text, 0), 0);
    }

    #[test]
    fn option_right_goes_to_the_end_of_the_word_or_the_next_one() {
        let text = "hello, big world";
        assert_eq!(next_word_end(text, 0), 5);
        assert_eq!(next_word_end(text, 5), 10);
        assert_eq!(next_word_end(text, 8), 10);
        assert_eq!(next_word_end(text, 11), 16);
        assert_eq!(next_word_end(text, 16), 16);
    }

    #[test]
    fn a_contraction_is_one_word() {
        assert_eq!(next_word_end("don't stop", 0), 5);
        assert_eq!(previous_word_start("don't stop", 5), 0);
    }

    #[test]
    fn paragraphs_are_walked_one_at_a_time() {
        let text = "one\ntwo\nthree";
        assert_eq!(previous_paragraph_start(text, 6), 4);
        assert_eq!(previous_paragraph_start(text, 4), 0);
        assert_eq!(next_paragraph_end(text, 4), 7);
        assert_eq!(next_paragraph_end(text, 7), 13);
    }

    #[test]
    fn a_run_of_typing_is_undone_as_one_step() {
        let mut history = History::default();
        let mut text = String::new();
        for letter in ["h", "i"] {
            let at = text.len();
            history.record(EditKind::Typing, &text, at..at, at);
            text.push_str(letter);
            history.edited(EditKind::Typing, text.len());
        }
        let (back, _) = history.undo(&text, 2..2).unwrap();
        assert_eq!(back, "");
        let (forward, _) = history.redo(&back, 0..0).unwrap();
        assert_eq!(forward, "hi");
    }

    #[test]
    fn moving_the_caret_ends_the_run() {
        let mut history = History::default();
        history.record(EditKind::Typing, "", 0..0, 0);
        history.edited(EditKind::Typing, 1);
        // The next letter is typed somewhere else.
        history.record(EditKind::Typing, "a", 0..0, 0);
        history.edited(EditKind::Typing, 1);
        let (back, _) = history.undo("ba", 1..1).unwrap();
        assert_eq!(back, "a");
    }

    #[test]
    fn a_paste_is_a_step_of_its_own() {
        let mut history = History::default();
        history.record(EditKind::Typing, "", 0..0, 0);
        history.edited(EditKind::Typing, 1);
        history.record(EditKind::Other, "a", 1..1, 1);
        history.edited(EditKind::Other, 6);
        let (back, _) = history.undo("apaste", 6..6).unwrap();
        assert_eq!(back, "a");
    }
}
