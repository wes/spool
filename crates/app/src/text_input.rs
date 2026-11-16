//! A single-line text field.
//!
//! gpui deliberately ships no widget library — it gives you a text system, a
//! focus model and an IME bridge, and you assemble the control you want. This
//! is the minimum honest version of that: a real input with selection, mouse
//! placement and input-method support, which the search box and the command
//! palette both reuse.
//!
//! Adapted from gpui's `input` example.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, fill, point,
    prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::caret::{Blink, Blinks, CARET_WIDTH};
use crate::text_motion::{self, EditKind, History};
use crate::theme::{Theme, ThemeAccess};

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        SelectToStart,
        SelectToEnd,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToStart,
        DeleteToEnd,
        Undo,
        Redo,
        Paste,
        Cut,
        Copy,
        ShowCharacterPalette,
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding as K;
    let ctx = Some("TextInput");
    cx.bind_keys([
        K::new("backspace", Backspace, ctx),
        K::new("delete", Delete, ctx),
        K::new("left", Left, ctx),
        K::new("right", Right, ctx),
        K::new("shift-left", SelectLeft, ctx),
        K::new("shift-right", SelectRight, ctx),
        K::new("cmd-a", SelectAll, ctx),
        K::new("ctrl-a", Home, ctx),
        K::new("ctrl-e", End, ctx),
        K::new("home", Home, ctx),
        K::new("end", End, ctx),
        // What a Mac text field does with the rest of the keyboard: ⌘ for
        // the whole line, ⌥ for a word, and shift on either to select.
        K::new("cmd-left", Home, ctx),
        K::new("cmd-right", End, ctx),
        K::new("cmd-up", Home, ctx),
        K::new("cmd-down", End, ctx),
        K::new("shift-home", SelectToStart, ctx),
        K::new("shift-end", SelectToEnd, ctx),
        K::new("cmd-shift-left", SelectToStart, ctx),
        K::new("cmd-shift-right", SelectToEnd, ctx),
        K::new("cmd-shift-up", SelectToStart, ctx),
        K::new("cmd-shift-down", SelectToEnd, ctx),
        K::new("alt-left", WordLeft, ctx),
        K::new("alt-right", WordRight, ctx),
        K::new("alt-shift-left", SelectWordLeft, ctx),
        K::new("alt-shift-right", SelectWordRight, ctx),
        K::new("alt-backspace", DeleteWordLeft, ctx),
        K::new("alt-delete", DeleteWordRight, ctx),
        K::new("cmd-backspace", DeleteToStart, ctx),
        K::new("cmd-delete", DeleteToEnd, ctx),
        K::new("ctrl-k", DeleteToEnd, ctx),
        K::new("ctrl-b", Left, ctx),
        K::new("ctrl-f", Right, ctx),
        K::new("ctrl-h", Backspace, ctx),
        K::new("ctrl-d", Delete, ctx),
        K::new("cmd-z", Undo, ctx),
        K::new("cmd-shift-z", Redo, ctx),
        K::new("cmd-v", Paste, ctx),
        K::new("cmd-x", Cut, ctx),
        K::new("cmd-c", Copy, ctx),
        K::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
    ]);
}

/// Emitted whenever the content changes, so an owner can re-run a query
/// without polling.
pub struct InputChanged(pub SharedString);

/// Emitted when a key asks to move off the front of the field.
///
/// A field on its own has nothing to the left of its first character, so these
/// announce something that cannot happen and nobody listens. A field with
/// something beside it does: the recipient field puts pills before the caret,
/// and backspace at the start of the token has to reach the last of them
/// rather than doing nothing.
///
/// Announced rather than acted on, because this widget has no idea what is
/// next to it.
pub enum Edge {
    /// Backspace, with the caret at the start and nothing selected.
    BackspaceAtStart,
    /// Left arrow, with the caret at the start.
    LeftAtStart,
    /// Right arrow, with the caret at the start of an *empty* field.
    ///
    /// Emptiness is the condition that makes this safe to announce rather than
    /// act on: with no text there is no caret movement being swallowed, so a
    /// field beside this one is free to treat it as its own. With text in it,
    /// right means what it always means and no event is emitted.
    RightAtStart,
}

/// What a masked field shows instead of its content.
///
/// Multi-byte, which is why every offset handed to the shaped line has to be
/// translated rather than used directly.
const MASK: char = '\u{2022}';

pub struct TextInput {
    pub focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    /// What to show instead when the field is too narrow for the real one.
    ///
    /// A placeholder is a sentence about what the field does, and a sentence
    /// cut off mid-word by the edge of a box reads as a rendering fault. This
    /// is the same thing said in one word.
    short_placeholder: Option<SharedString>,
    /// Draws bullets instead of the text. Only the drawing changes: the
    /// content, the selection and the clipboard behave exactly as they would
    /// in a visible field.
    masked: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// The IME's in-progress composition, underlined until committed.
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the text is scrolled left inside the field, in pixels.
    ///
    /// A single-line field is a window onto a line that can be longer than it
    /// is: the text is clipped to the box — which is the whole of what stops a
    /// long placeholder painting across the toolbar — and this is what keeps
    /// the caret on the visible side of that clip while you type.
    scroll_x: Pixels,
    is_selecting: bool,
    /// What a drag extends by, after a double or triple click.
    word_drag: Option<Range<usize>>,
    history: History,
    blink: Blink,
}

impl EventEmitter<InputChanged> for TextInput {}
impl EventEmitter<Edge> for TextInput {}

impl Blinks for TextInput {
    fn blink(&mut self) -> &mut Blink {
        &mut self.blink
    }
}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            short_placeholder: None,
            masked: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            is_selecting: false,
            word_drag: None,
            history: History::default(),
            blink: Blink::default(),
        }
    }

    /// A field whose content is drawn as bullets, for passwords and tokens.
    pub fn masked(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self { masked: true, ..Self::new(placeholder, cx) }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// The short placeholder on its own, for a field built with `new`.
    pub fn set_short_placeholder(&mut self, short: impl Into<SharedString>) {
        self.short_placeholder = Some(short.into());
    }

    /// What is drawn: the content, or one bullet per grapheme.
    ///
    /// Per grapheme rather than per character so that an accented letter or an
    /// emoji is one bullet, matching what it took to type and what one
    /// backspace will remove.
    fn display_text(&self) -> SharedString {
        if !self.masked {
            return self.content.clone();
        }
        self.content
            .graphemes(true)
            .map(|_| MASK)
            .collect::<String>()
            .into()
    }

    /// Translates a content offset into an offset in the displayed text.
    ///
    /// The shaped line is built from the display text, so every offset that
    /// reaches it — the caret, the selection edges — has to come through here
    /// or a masked field draws its caret in the wrong place.
    fn content_to_display(&self, offset: usize) -> usize {
        match self.masked {
            true => content_to_display(&self.content, offset),
            false => offset,
        }
    }

    /// The inverse, for turning a click back into a cursor position.
    fn display_to_content(&self, offset: usize) -> usize {
        match self.masked {
            true => display_to_content(&self.content, offset),
            false => offset,
        }
    }

    pub fn set_content(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.history.clear();
        let end = self.content.len();
        self.selected_range = end..end;
        self.marked_range = None;
        self.blink.wake(cx);
        cx.emit(InputChanged(self.content.clone()));
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_content("", cx);
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if self.cursor_offset() == 0 {
                cx.emit(Edge::LeftAtStart);
                return;
            }
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if self.content.is_empty() {
                cx.emit(Edge::RightAtStart);
                return;
            }
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_start(self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_end(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_start(self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_end(self.cursor_offset()), cx);
    }

    /// A masked field has no words to show, so it has none to stop at
    /// either — the same as a password field anywhere else on a Mac.
    fn previous_word_start(&self, offset: usize) -> usize {
        match self.masked {
            true => 0,
            false => text_motion::previous_word_start(&self.content, offset),
        }
    }

    fn next_word_end(&self, offset: usize) -> usize {
        match self.masked {
            true => self.content.len(),
            false => text_motion::next_word_end(&self.content, offset),
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() && self.cursor_offset() == 0 {
            cx.emit(Edge::BackspaceAtStart);
            return;
        }
        let target = self.previous_boundary(self.cursor_offset());
        self.delete_back_to(target, EditKind::Deleting, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor_offset());
        self.delete_forward_to(target, EditKind::Deleting, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_word_start(self.cursor_offset());
        self.delete_back_to(target, EditKind::Other, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_word_end(self.cursor_offset());
        self.delete_forward_to(target, EditKind::Other, cx);
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_back_to(0, EditKind::Other, cx);
    }

    fn delete_to_end(&mut self, _: &DeleteToEnd, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.content.len();
        self.delete_forward_to(end, EditKind::Other, cx);
    }

    /// Deletes the selection, or from `target` up to the caret when nothing
    /// is selected.
    fn delete_back_to(&mut self, target: usize, kind: EditKind, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = match self.selected_range.is_empty() {
            true => target.min(cursor)..cursor,
            false => self.selected_range.clone(),
        };
        self.edit(range, "", kind, cx);
    }

    fn delete_forward_to(&mut self, target: usize, kind: EditKind, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = match self.selected_range.is_empty() {
            true => cursor..target.max(cursor),
            false => self.selected_range.clone(),
        };
        self.edit(range, "", kind, cx);
    }

    /// Every change to the text goes through here, which is what lets ⌘Z
    /// take any of them back.
    fn edit(&mut self, range: Range<usize>, new_text: &str, kind: EditKind, cx: &mut Context<Self>) {
        let range = self.safe_range(range);
        if range.is_empty() && new_text.is_empty() {
            return;
        }
        let kind = match self.selected_range.is_empty() {
            true => kind,
            false => EditKind::Other,
        };
        // A composition being committed was recorded when it began.
        if self.marked_range.is_none() {
            self.history.record(kind, &self.content, self.selected_range.clone(), self.cursor_offset());
        }

        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range.take();
        self.history.edited(kind, cursor);

        self.blink.wake(cx);
        cx.emit(InputChanged(self.content.clone()));
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((content, selection)) =
            self.history.undo(&self.content, self.selected_range.clone())
        {
            self.restore(content, selection, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((content, selection)) =
            self.history.redo(&self.content, self.selected_range.clone())
        {
            self.restore(content, selection, cx);
        }
    }

    fn restore(&mut self, content: String, selection: Range<usize>, cx: &mut Context<Self>) {
        self.content = content.into();
        self.selected_range = self.safe_range(selection);
        self.selection_reversed = false;
        self.marked_range = None;
        self.blink.wake(cx);
        cx.emit(InputChanged(self.content.clone()));
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let range = self.marked_range.clone().unwrap_or_else(|| self.selected_range.clone());
            self.edit(range, &text.replace('\n', " "), EditKind::Other, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            let range = self.selected_range.clone();
            self.edit(range, "", EditKind::Other, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        // A double click takes the word, a triple the whole field, and a drag
        // after either grows by that much — what every Mac text field does.
        self.word_drag = match event.click_count {
            0 | 1 => None,
            2 if !self.masked => Some(word_unit(&self.content, offset)),
            _ => Some(0..self.content.len()),
        };
        if let Some(unit) = self.word_drag.clone() {
            self.selected_range = unit;
            self.selection_reversed = false;
            self.blink.wake(cx);
            cx.notify();
        } else if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
        self.word_drag = None;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        let offset = self.index_for_mouse_position(event.position);
        let Some(anchor) = self.word_drag.clone() else {
            self.select_to(offset, cx);
            return;
        };
        let unit = word_unit(&self.content, offset);
        let (range, reversed) = if unit.start < anchor.start {
            (unit.start..anchor.end, true)
        } else {
            (anchor.start..unit.end.max(anchor.end), false)
        };
        if range != self.selected_range || reversed != self.selection_reversed {
            self.selected_range = range;
            self.selection_reversed = reversed;
            self.blink.wake(cx);
            cx.notify();
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.blink.wake(cx);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.blink.wake(cx);
        cx.notify();
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.display_to_content(
            line.closest_index_for_x(position.x - bounds.left() + self.scroll_x),
        )
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let (mut utf8, mut utf16) = (0usize, 0usize);
        for ch in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let (mut utf8, mut utf16) = (0usize, 0usize);
        for ch in self.content.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// A range that is safe to slice `content` with.
    ///
    /// Every range reaching the input handler is either the system's or one
    /// this field wrote down earlier, and both can be wrong: macOS asks about
    /// text that was there a moment ago, and a stored selection outlives the
    /// content it described if anything replaces that content behind it. The
    /// cost of trusting one is not a bad edit — it is `abort()`, because these
    /// handlers run inside Cocoa callbacks where a panic cannot unwind and the
    /// message is lost with the process.
    fn safe_range(&self, range: Range<usize>) -> Range<usize> {
        let start = floor_boundary(&self.content, range.start);
        let end = floor_boundary(&self.content, range.end);
        start.min(end)..start.max(end)
    }

    /// Grapheme-aware, so an emoji or a combining accent deletes as one unit.
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }
}

/// The word a double click at `offset` selects: the one under it, or the
/// space or punctuation there if that is what was clicked.
fn word_unit(text: &str, offset: usize) -> Range<usize> {
    text.split_word_bound_indices()
        .map(|(start, segment)| start..start + segment.len())
        .find(|range| range.contains(&offset))
        .or_else(|| {
            // Past the end of the text: its last word.
            text.split_word_bound_indices().last().map(|(start, s)| start..start + s.len())
        })
        .unwrap_or(offset..offset)
}

/// The nearest character boundary at or before `offset`, and never past the
/// end of `text`.
///
/// Rust slices by byte, and a byte offset that lands inside a character is a
/// panic rather than an error. Rounding down rather than up keeps an edit
/// where the caret was rather than one character further on.
pub(crate) fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.safe_range(self.range_from_utf16(&range_utf16));
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.edit(range, new_text, EditKind::Typing, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.safe_range(
            range_utf16
                .as_ref()
                .map(|r| self.range_from_utf16(r))
                .or_else(|| self.marked_range.clone())
                .unwrap_or_else(|| self.selected_range.clone()),
        );

        // The composition's first keystroke is where the step starts; the rest
        // of it rewrites the same marked text and is part of that step.
        if self.marked_range.is_none() {
            self.history.record(
                EditKind::Typing,
                &self.content,
                self.selected_range.clone(),
                self.cursor_offset(),
            );
        }
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.end)
            .unwrap_or_else(|| {
                let cursor = range.start + new_text.len();
                cursor..cursor
            });

        self.blink.wake(cx);
        cx.emit(InputChanged(self.content.clone()));
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.safe_range(self.range_from_utf16(&range_utf16));
        // Through the mask on the way out: the shaped line is the bullets, so
        // a content offset means nothing to it. Without this, an input method
        // composing in a masked field is handed the rectangle of the wrong
        // character to hang its candidate window from.
        let (start, end) =
            (self.content_to_display(range.start), self.content_to_display(range.end));
        Some(Bounds::from_corners(
            point(bounds.left() + last_layout.x_for_index(start) - self.scroll_x, bounds.top()),
            point(bounds.left() + last_layout.x_for_index(end) - self.scroll_x, bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        // And back through it on the way in: what the line answers with is an
        // offset into the bullets.
        let ix = last_layout.index_for_x(point.x - line_point.x + self.scroll_x)?;
        Some(self.offset_to_utf16(self.display_to_content(ix)))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Draws the field's text, selection and caret, and installs the IME handler.
///
/// This is a custom [`Element`] rather than a `div` because a caret has to be
/// positioned from the shaped line, which only exists after layout.
struct TextElement {
    input: Entity<TextInput>,
    theme: Theme,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    /// How far the line was shifted left to keep the caret in view.
    scroll: Pixels,
}

/// Where a single-line field's view should sit, given where the caret is.
///
/// Classic single-line scrolling: the view only moves when the caret would
/// otherwise leave it, and then only far enough to bring it back. Text that
/// fits is never scrolled, and a field that has been scrolled and then shrinks
/// is pulled back so there is no blank gap at its right edge.
fn scroll_for(current: Pixels, caret: Pixels, line: Pixels, width: Pixels) -> Pixels {
    // Room kept between the caret and the edge it is approaching, so the
    // character being typed is next to something rather than against a wall.
    const MARGIN: Pixels = px(8.);

    if line <= width {
        return px(0.);
    }

    let furthest = (line - width).max(px(0.));
    let mut scroll = current.min(furthest).max(px(0.));

    // Off the right edge: bring the caret back inside, with the margin.
    if caret - scroll > width - MARGIN {
        scroll = caret - width + MARGIN;
    }
    // Off the left: the same, from the other side.
    if caret - scroll < MARGIN {
        scroll = caret - MARGIN;
    }

    scroll.min(furthest).max(px(0.))
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let selected_range =
            input.content_to_display(input.selected_range.start)
                ..input.content_to_display(input.selected_range.end);
        let cursor = input.content_to_display(input.cursor_offset());
        let style = window.text_style();

        let showing_placeholder = input.content.is_empty();
        let (display_text, color) = if showing_placeholder {
            (input.placeholder.clone(), self.theme.text_faint)
        } else {
            (input.display_text(), style.color)
        };

        let base = TextRun {
            len: display_text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // The IME's uncommitted text is underlined, which is the convention
        // every platform's input method expects to see.
        // A masked field draws one uniform run: the underline that marks
        // uncommitted IME text would be positioned against content offsets
        // that no longer line up, and revealing composition state in a
        // password field is not wanted anyway.
        let runs = match input.marked_range.as_ref().filter(|_| !input.masked) {
            Some(marked) => vec![
                TextRun { len: marked.start, ..base.clone() },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(base.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..base.clone()
                },
                TextRun { len: display_text.len() - marked.end, ..base },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            None => vec![base],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        // A placeholder that does not fit is replaced rather than clipped:
        // the content of a field has to be shown however long it is, but the
        // prompt for an empty one is ours to choose, and half a sentence
        // against the edge of the box says nothing the short one does not.
        let line = match input.short_placeholder.clone() {
            Some(short) if showing_placeholder && line.width > bounds.size.width => {
                let run = TextRun { len: short.len(), ..runs[0].clone() };
                window.text_system().shape_line(short, font_size, &[run], None)
            }
            _ => line,
        };

        // How far the line is shifted left, so the caret is inside the box
        // even when the text is longer than it. Kept between frames: a field
        // scrolled to the end has to stay there while the caret sits still,
        // and recomputing it from the caret alone would snap the view about
        // as the cursor moves through the middle of a long line.
        let scroll = scroll_for(
            self.input.read(cx).scroll_x,
            line.x_for_index(cursor),
            line.width,
            bounds.size.width,
        );
        self.input.update(cx, |input, _| input.scroll_x = scroll);

        let left = bounds.left() - scroll;

        let (selection, cursor) = if selected_range.is_empty() {
            let x = line.x_for_index(cursor);
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(left + x, bounds.top()),
                        size(CARET_WIDTH, bounds.bottom() - bounds.top()),
                    ),
                    self.theme.accent,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(left + line.x_for_index(selected_range.start), bounds.top()),
                        point(left + line.x_for_index(selected_range.end), bounds.bottom()),
                    ),
                    self.theme.text_selection,
                )),
                None,
            )
        };

        PrepaintState { line: Some(line), cursor, selection, scroll }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let line = prepaint.line.take().unwrap();
        let scroll = prepaint.scroll;
        let selection = prepaint.selection.take();
        let cursor = prepaint.cursor.take();
        // Only the focused field shows a caret.
        let focused = focus_handle.is_focused(window);
        // Paint is where the blink hears about focus: a field is built before
        // there is a window, so there is nowhere earlier to ask.
        let blink_on = self.input.update(cx, |input, cx| {
            input.blink.set_focused(focused, cx);
            input.blink.visible()
        });

        // Everything the field draws is clipped to the field. Without this a
        // line longer than its box — a placeholder in a search field squeezed
        // by a narrow window, most of all — paints straight across whatever
        // is beside it and off the edge of the window.
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = selection {
                window.paint_quad(selection);
            }

            line.paint(
                point(bounds.origin.x - scroll, bounds.origin.y),
                window.line_height(),
                window,
                cx,
            )
            .ok();

            if focused && blink_on && let Some(cursor) = cursor {
                window.paint_quad(cursor);
            }
        });

        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read from the global rather than taken as a parameter: the field is
        // rendered by its owning entity, which has no theme to hand it.
        let theme = cx.theme().clone();

        gpui::div()
            .flex()
            .flex_1()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::delete_to_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .text_color(theme.text)
            .child(TextElement { input: cx.entity(), theme })
    }
}

/// Content offset to masked-display offset, and back.
///
/// Free functions rather than methods so the arithmetic can be tested without
/// a window. An off-by-one here puts the caret of a password field in the
/// wrong place, which is not something a code review can see.
fn content_to_display(content: &str, offset: usize) -> usize {
    // Snap to a character boundary: slicing at an interior byte panics, and a
    // caller can hand over any offset it likes.
    let offset = offset.min(content.len());
    let offset = (0..=offset).rev().find(|ix| content.is_char_boundary(*ix)).unwrap_or(0);
    content[..offset].graphemes(true).count() * MASK.len_utf8()
}

fn display_to_content(content: &str, offset: usize) -> usize {
    content
        .grapheme_indices(true)
        .nth(offset / MASK.len_utf8())
        .map(|(ix, _)| ix)
        // Past the last bullet is the end of the content, which is where a
        // click to the right of the text should put the caret.
        .unwrap_or(content.len())
}

#[cfg(test)]
mod tests {
    use super::{MASK, content_to_display, display_to_content, floor_boundary, scroll_for};
    use gpui::px;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn a_field_that_fits_its_text_is_never_scrolled() {
        assert_eq!(scroll_for(px(0.), px(20.), px(80.), px(200.)), px(0.));
        // Even if it had been scrolled before the text got shorter.
        assert_eq!(scroll_for(px(50.), px(20.), px(80.), px(200.)), px(0.));
    }

    #[test]
    fn typing_past_the_right_edge_brings_the_caret_back_into_view() {
        // A caret at 300 in a box 200 wide: the view moves just far enough to
        // put it inside, with the margin.
        let scroll = scroll_for(px(0.), px(300.), px(400.), px(200.));
        assert_eq!(scroll, px(108.));
        assert!(px(300.) - scroll < px(200.), "the caret is inside the box");
    }

    #[test]
    fn moving_back_to_the_start_scrolls_the_view_back() {
        let scroll = scroll_for(px(150.), px(10.), px(400.), px(200.));
        assert_eq!(scroll, px(2.), "the caret, less the margin");
    }

    #[test]
    fn a_caret_already_in_view_leaves_the_view_where_it_is() {
        // The reason this is remembered rather than recomputed: a cursor
        // moving through the middle of a long line must not drag the text
        // about under it.
        assert_eq!(scroll_for(px(100.), px(150.), px(400.), px(200.)), px(100.));
    }

    #[test]
    fn the_view_never_scrolls_past_the_end_of_the_line() {
        // What a stale offset would do after the text was cut short: a field
        // showing blank space where its own text should be.
        assert_eq!(scroll_for(px(900.), px(390.), px(400.), px(200.)), px(200.));
    }

    #[test]
    fn an_offset_inside_a_character_rounds_back_to_where_it_started() {
        // The offsets macOS can hand back, and the ones a stale selection
        // holds, are bytes into text that may have changed underneath them.
        // Every one of these would panic as a slice index.
        let text = "a\u{00e9}b";  // 'a', two bytes of 'é', 'b'
        assert_eq!(floor_boundary(text, 0), 0);
        assert_eq!(floor_boundary(text, 1), 1);
        assert_eq!(floor_boundary(text, 2), 1, "inside the accented letter");
        assert_eq!(floor_boundary(text, 3), 3);
        assert_eq!(floor_boundary(text, 4), 4, "the end");
        assert_eq!(floor_boundary(text, 40), 4, "past the end");
    }

    #[test]
    fn an_emoji_is_never_cut_in_half() {
        let text = "\u{1F680}";  // four bytes, one character
        for inside in 1..4 {
            assert_eq!(floor_boundary(text, inside), 0);
        }
        assert_eq!(floor_boundary(text, 4), 4);
    }

    #[test]
    fn nothing_can_be_asked_of_an_empty_field() {
        assert_eq!(floor_boundary("", 0), 0);
        assert_eq!(floor_boundary("", 9), 0);
    }

    #[test]
    fn each_ascii_character_becomes_exactly_one_bullet() {
        let content = "hunter2";
        for i in 0..=content.len() {
            assert_eq!(content_to_display(content, i), i * MASK.len_utf8());
        }
    }

    #[test]
    fn multi_byte_characters_still_map_one_to_one() {
        // Three graphemes, six bytes. Using the offset unchanged — which is
        // what a visible field does — would put the caret three bullets past
        // the end of this.
        let content = "a\u{e9}\u{4e2d}";
        assert_eq!(content.len(), 6);
        assert_eq!(content_to_display(content, 0), 0);
        assert_eq!(content_to_display(content, 1), MASK.len_utf8());
        assert_eq!(content_to_display(content, 3), 2 * MASK.len_utf8());
        assert_eq!(content_to_display(content, 6), 3 * MASK.len_utf8());
    }

    #[test]
    fn a_combining_accent_is_one_bullet_rather_than_two() {
        // "e" followed by U+0301: one grapheme, and one backspace removes it.
        assert_eq!(content_to_display("e\u{301}", 3), MASK.len_utf8());
    }

    #[test]
    fn the_mapping_round_trips_at_every_grapheme_boundary() {
        for content in ["", "hunter2", "a\u{e9}\u{4e2d}", "e\u{301}x", "\u{1f512}pw"] {
            for (ix, _) in content.grapheme_indices(true) {
                assert_eq!(
                    display_to_content(content, content_to_display(content, ix)),
                    ix,
                    "{content:?} at {ix}"
                );
            }
        }
    }

    #[test]
    fn a_click_past_the_end_lands_at_the_end() {
        assert_eq!(display_to_content("hunter2", 9_999), 7);
        assert_eq!(display_to_content("", 0), 0);
    }

    #[test]
    fn an_offset_inside_a_character_snaps_instead_of_panicking() {
        // Nothing should hand over an interior byte, but a panic in a text
        // field is a crash in the app.
        let content = "\u{4e2d}\u{6587}";
        assert_eq!(content_to_display(content, 1), 0);
        assert_eq!(content_to_display(content, 2), 0);
        assert_eq!(content_to_display(content, 3), MASK.len_utf8());
        assert_eq!(content_to_display(content, 99), 2 * MASK.len_utf8());
    }
}
