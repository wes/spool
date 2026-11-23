//! A wrapping, multi-line text editor.
//!
//! [`TextInput`](crate::text_input::TextInput) is one shaped line with a caret
//! on it. Almost none of that survives contact with wrapping: a caret has a row
//! as well as a column, "up" is a question about geometry rather than about
//! byte offsets, and a selection covers parts of several rows. So this is a
//! separate widget rather than a mode.
//!
//! Two things carry most of the weight:
//!
//! * gpui's `shape_text` returns one [`WrappedLine`] per logical line, each of
//!   which knows its own wrap boundaries and can map between a byte offset and
//!   a point. Vertical movement is then a geometry question the layout can
//!   answer, rather than something this file has to derive.
//! * Selection is painted by giving the shaped runs a background colour, not by
//!   computing quads. That makes a selection spanning five wrapped rows exactly
//!   as much work as one spanning three characters.
//!
//! The element grows to fit its content and expects an ancestor to scroll,
//! which is why there is no viewport arithmetic here beyond one thing: an
//! owner that hands over that ancestor's [`ScrollHandle`] with
//! [`TextArea::follow_with`] gets it scrolled to keep the caret in sight.

use std::ops::Range;

use gpui::{
    Hsla,
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontWeight, GlobalElementId,
    LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollHandle, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, actions, fill, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::caret::{Blink, Blinks, CARET_WIDTH};
use crate::text_motion::{self, EditKind, History};
use crate::theme::{Theme, ThemeAccess};

actions!(
    text_area,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        LineStart,
        LineEnd,
        DocumentStart,
        DocumentEnd,
        SelectToLineStart,
        SelectToLineEnd,
        SelectToDocumentStart,
        SelectToDocumentEnd,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        ParagraphUp,
        ParagraphDown,
        SelectParagraphUp,
        SelectParagraphDown,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        Undo,
        Redo,
        Newline,
        Paste,
        Cut,
        Copy,
        ShowCharacterPalette,
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding as K;
    let ctx = Some("TextArea");

    cx.bind_keys([
        K::new("backspace", Backspace, ctx),
        K::new("delete", Delete, ctx),
        K::new("left", Left, ctx),
        K::new("right", Right, ctx),
        K::new("up", Up, ctx),
        K::new("down", Down, ctx),
        K::new("shift-left", SelectLeft, ctx),
        K::new("shift-right", SelectRight, ctx),
        K::new("shift-up", SelectUp, ctx),
        K::new("shift-down", SelectDown, ctx),
        K::new("cmd-a", SelectAll, ctx),
        K::new("home", LineStart, ctx),
        K::new("end", LineEnd, ctx),
        K::new("cmd-left", LineStart, ctx),
        K::new("cmd-right", LineEnd, ctx),
        K::new("ctrl-a", LineStart, ctx),
        K::new("ctrl-e", LineEnd, ctx),
        K::new("cmd-up", DocumentStart, ctx),
        K::new("cmd-down", DocumentEnd, ctx),
        // The rest of what a Mac text view does with the arrows: by word with
        // option, by paragraph with option up and down, and shift on any of
        // them extending the selection rather than moving the caret.
        K::new("shift-home", SelectToLineStart, ctx),
        K::new("shift-end", SelectToLineEnd, ctx),
        K::new("cmd-shift-left", SelectToLineStart, ctx),
        K::new("cmd-shift-right", SelectToLineEnd, ctx),
        K::new("cmd-shift-up", SelectToDocumentStart, ctx),
        K::new("cmd-shift-down", SelectToDocumentEnd, ctx),
        K::new("alt-left", WordLeft, ctx),
        K::new("alt-right", WordRight, ctx),
        K::new("alt-shift-left", SelectWordLeft, ctx),
        K::new("alt-shift-right", SelectWordRight, ctx),
        K::new("alt-up", ParagraphUp, ctx),
        K::new("alt-down", ParagraphDown, ctx),
        K::new("alt-shift-up", SelectParagraphUp, ctx),
        K::new("alt-shift-down", SelectParagraphDown, ctx),
        K::new("alt-backspace", DeleteWordLeft, ctx),
        K::new("alt-delete", DeleteWordRight, ctx),
        K::new("cmd-backspace", DeleteToLineStart, ctx),
        K::new("cmd-delete", DeleteToLineEnd, ctx),
        // The Emacs keys every Cocoa text view answers to.
        K::new("ctrl-b", Left, ctx),
        K::new("ctrl-f", Right, ctx),
        K::new("ctrl-p", Up, ctx),
        K::new("ctrl-n", Down, ctx),
        K::new("ctrl-h", Backspace, ctx),
        K::new("ctrl-d", Delete, ctx),
        K::new("ctrl-k", DeleteToLineEnd, ctx),
        K::new("cmd-z", Undo, ctx),
        K::new("cmd-shift-z", Redo, ctx),
        // `enter` inserts a newline here rather than confirming anything, which
        // is why the composer cannot reuse the window's `Confirm` binding.
        K::new("enter", Newline, ctx),
        K::new("cmd-v", Paste, ctx),
        K::new("cmd-x", Cut, ctx),
        K::new("cmd-c", Copy, ctx),
        K::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
        // Text that is only there to be read: copying out of it is all there
        // is to do. Everything else — the arrows, `j`, `k`, `backspace` —
        // falls through to the window, which is where the reader wants it.
        K::new("cmd-c", Copy, Some("ReadOnlyText")),
        K::new("cmd-a", SelectAll, Some("ReadOnlyText")),
    ]);
}

/// Emitted on every content change, so an owner can react without polling.
///
/// Carries nothing: an owner that needs the text reads it from the entity,
/// and shipping a copy of the whole body on every keystroke would be waste.
pub struct AreaChanged;

/// An image was pasted into the body.
///
/// Emitted rather than handled, because a text area has no idea what to do
/// with a picture — it has no way to show one and no business attaching one.
/// The composer above it does, and this is how it finds out. A text area
/// nobody has wired this up on simply drops it, which is the same thing it did
/// before the event existed.
#[derive(Debug, Clone)]
pub struct ImagePasted(pub gpui::Image);

/// One logical line, shaped and placed.
struct Row {
    /// Byte offset in the content where this line starts.
    start: usize,
    line: WrappedLine,
    /// Distance from the top of the element to this line's first row.
    top: Pixels,
    /// Wrapped rows this logical line occupies — at least one.
    rows: usize,
}

impl Row {
    fn end(&self) -> usize {
        self.start + self.line.len()
    }

    fn height(&self, line_height: Pixels) -> Pixels {
        line_height * self.rows as f32
    }
}

#[derive(Default)]
struct Layout {
    rows: Vec<Row>,
    bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
}

impl Layout {
    /// Total height, which is what the element reports to its parent.
    ///
    /// An empty area still claims one line, so a composer does not collapse to
    /// nothing before anything is typed into it.
    fn height(&self) -> Pixels {
        self.rows
            .last()
            .map(|row| row.top + row.height(self.line_height))
            .unwrap_or(self.line_height)
    }
}

pub struct TextArea {
    pub focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// The IME's uncommitted composition, underlined until it commits.
    marked_range: Option<Range<usize>>,
    /// The x to aim for while moving vertically.
    ///
    /// Without it, walking down through a short line and back up lands you at
    /// the end of the short line rather than where you started — the behaviour
    /// every editor is expected not to have.
    goal_x: Option<Pixels>,
    /// The face and size to shape with, when the owner wants something other
    /// than the window's.
    ///
    /// Carried explicitly rather than inherited from the element it sits in,
    /// and that is not a preference: shaping happens inside a *measured
    /// layout* callback, which the layout pass runs after the enclosing
    /// `with_text_style` scope has been unwound. `window.text_style()` there
    /// reports the window's default, so a composer setting `.font_family()`
    /// and `.text_size()` on the div around this would be quietly ignored.
    style: AreaStyle,
    layout: Layout,
    is_selecting: bool,
    /// What a drag extends by: letters after a single click, whole words
    /// after a double click, whole lines after a triple.
    granularity: Granularity,
    /// The unit the click that started the drag landed on.
    ///
    /// A drag after a double click keeps that word selected however far back
    /// it goes, and grows by words from whichever side the mouse is on, so it
    /// needs to remember which word it started with.
    anchor: Range<usize>,
    blink: Blink,
    /// Set for text that is there to be read and copied, not edited.
    read_only: Option<ReadOnly>,
    history: History,
    /// The scrolling box this sits in, when its owner wants the caret kept
    /// in view. See [`TextArea::follow_with`].
    follow: Option<ScrollHandle>,
    /// Set by anything from the keyboard that moves the caret, and taken by
    /// the next prepaint, which scrolls `follow` to show it. A click does not
    /// set it: what was clicked is already on screen.
    reveal: bool,
}

/// How much one click selects, and so how much a drag after it grows by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Granularity {
    #[default]
    Char,
    Word,
    Line,
}

/// What a read-only area carries on top of its text.
///
/// The reader shows plain-text mail through one of these, which is what makes
/// it selectable: a `StyledText` per paragraph can be drawn but has no notion
/// of a selection, let alone one that runs from one paragraph into the next.
#[derive(Default)]
pub struct ReadOnly {
    /// Ranges drawn in a colour other than the area's ink, such as quoted
    /// text.
    pub colors: Vec<(Range<usize>, Hsla)>,
    /// Ranges that open a URL on a click that selected nothing.
    pub links: Vec<(Range<usize>, String)>,
    /// Where focus goes back to once a click leaves nothing selected.
    ///
    /// The area only takes focus when there is something to copy. Keeping it
    /// after a plain click would leave the message list's one-letter
    /// shortcuts dead until the list was clicked again, for a pane the person
    /// only clicked to read.
    pub return_focus: Option<FocusHandle>,
}

/// What a `TextArea` shapes with, over the window's own text style.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AreaStyle {
    pub font_family: Option<SharedString>,
    pub font_size: Option<Pixels>,
    pub font_weight: Option<FontWeight>,
    /// The leading, when it has to match text drawn some other way beside
    /// the area. `None` is proportional to the size.
    pub line_height: Option<Pixels>,
    /// The caret and the fill behind selected text, for an area drawn on a
    /// surface that is not the window's own.
    ///
    /// The composer's body is the case these exist for: it is a white page in
    /// every theme, so the two colours the area invents for itself cannot come
    /// from the window's palette. On the dark theme that palette draws a white
    /// caret, which on a white page is nothing at all, and a selection fill
    /// dark enough for near-white text, which the page's near-black ink is
    /// then unreadable on. `None` takes them from the theme, which is right
    /// for an area sitting on the window's own surface.
    pub caret: Option<Hsla>,
    pub selection: Option<Hsla>,
    /// The text colour, when the window's default is not the right one.
    ///
    /// For the same reason as the face: the element around the area cannot
    /// pass its colour through the measured layout.
    pub ink: Option<Hsla>,
}

impl EventEmitter<AreaChanged> for TextArea {}
impl EventEmitter<ImagePasted> for TextArea {}

impl Blinks for TextArea {
    fn blink(&mut self) -> &mut Blink {
        &mut self.blink
    }
}

impl TextArea {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            goal_x: None,
            style: AreaStyle::default(),
            layout: Layout::default(),
            is_selecting: false,
            granularity: Granularity::Char,
            anchor: 0..0,
            blink: Blink::default(),
            read_only: None,
            history: History::default(),
            follow: None,
            reveal: false,
        }
    }

    /// Keeps the caret in view of `scroll`, the handle tracked by the box
    /// that scrolls this area, as typing and the arrow keys move it.
    pub fn follow_with(&mut self, scroll: ScrollHandle) {
        self.follow = Some(scroll);
    }

    /// An area for reading: selectable and copyable, with no caret and no
    /// way to change the text.
    ///
    /// Takes the window so it can let go of its selection when focus moves
    /// elsewhere. A selection left behind in a pane nobody is looking at
    /// looks like it is still what copy would take, when it is not.
    pub fn read_only(
        text: impl Into<SharedString>,
        read_only: ReadOnly,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut area = Self::new("", cx);
        area.content = text.into();
        area.read_only = Some(read_only);
        cx.on_blur(&area.focus_handle, window, |area, _, cx| {
            area.selected_range = 0..0;
            area.selection_reversed = false;
            cx.notify();
        })
        .detach();
        area
    }

    /// Shapes with this face and size instead of the window's.
    pub fn set_style(&mut self, style: AreaStyle, cx: &mut Context<Self>) {
        if self.style != style {
            self.style = style;
            cx.notify();
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the whole text. What was there cannot be got back with ⌘Z:
    /// the owner replaced it, and owns taking that back.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.history.clear();
        let end = self.content.len();
        self.selected_range = end..end;
        self.marked_range = None;
        self.goal_x = None;
        self.blink.wake(cx);
        cx.emit(AreaChanged);
        cx.notify();
    }

    /// What is selected, as byte offsets into [`Self::text`]. Empty when
    /// there is only a caret.
    pub fn selection(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// Selects a range, for an owner that has just put something there and
    /// wants it seen.
    pub fn select(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.selected_range = self.safe_range(range);
        self.selection_reversed = false;
        self.goal_x = None;
        self.blink.wake(cx);
        cx.notify();
    }

    /// Puts the caret at the very start, where a reply wants it — above the
    /// quoted text rather than after it.
    pub fn move_to_start(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.goal_x = None;
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    // ---- horizontal movement ----

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor())
        } else {
            self.selected_range.start
        };
        self.move_to(target, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected_range.is_empty() {
            self.next_boundary(self.cursor())
        } else {
            self.selected_range.end
        };
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor());
        self.select_to(target, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor());
        self.select_to(target, cx);
    }

    // ---- vertical movement ----

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.offset_one_row(-1) {
            self.move_keeping_goal(target, cx);
        } else {
            self.move_to(0, cx);
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.offset_one_row(1) {
            self.move_keeping_goal(target, cx);
        } else {
            self.move_to(self.content.len(), cx);
        }
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.offset_one_row(-1).unwrap_or(0);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.offset_one_row(1).unwrap_or(self.content.len());
        self.select_to(target, cx);
    }

    /// The offset one wrapped row above or below the caret.
    ///
    /// Rows rather than logical lines: on a wrapped paragraph, "up" means the
    /// visual row above, which is what the caret is actually sitting on.
    fn offset_one_row(&mut self, direction: i32) -> Option<usize> {
        let line_height = self.layout.line_height;
        if line_height <= px(0.) {
            return None;
        }

        let here = self.point_for_offset(self.cursor())?;
        // The remembered column survives a run of vertical moves; a horizontal
        // move clears it.
        let goal_x = self.goal_x.unwrap_or(here.x);
        self.goal_x = Some(goal_x);

        let target_y = here.y + line_height * direction as f32;
        if target_y < px(0.) {
            return None;
        }
        let total = self.layout.rows.last().map(|r| r.top + r.height(line_height))?;
        if target_y >= total {
            return None;
        }

        Some(self.offset_for_point(point(goal_x, target_y)))
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.visual_line_start(self.cursor());
        self.move_to(target, cx);
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.visual_line_end(self.cursor());
        self.move_to(target, cx);
    }

    fn document_start(&mut self, _: &DocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_to_line_start(&mut self, _: &SelectToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.visual_line_start(self.cursor());
        self.select_to(target, cx);
    }

    fn select_to_line_end(&mut self, _: &SelectToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.visual_line_end(self.cursor());
        self.select_to(target, cx);
    }

    fn select_to_document_start(
        &mut self,
        _: &SelectToDocumentStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    fn select_to_document_end(
        &mut self,
        _: &SelectToDocumentEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.content.len(), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::previous_word_start(&self.content, self.cursor());
        self.move_to(target, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::next_word_end(&self.content, self.cursor());
        self.move_to(target, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::previous_word_start(&self.content, self.cursor());
        self.select_to(target, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::next_word_end(&self.content, self.cursor());
        self.select_to(target, cx);
    }

    fn paragraph_up(&mut self, _: &ParagraphUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::previous_paragraph_start(&self.content, self.cursor());
        self.move_to(target, cx);
    }

    fn paragraph_down(&mut self, _: &ParagraphDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::next_paragraph_end(&self.content, self.cursor());
        self.move_to(target, cx);
    }

    fn select_paragraph_up(&mut self, _: &SelectParagraphUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::previous_paragraph_start(&self.content, self.cursor());
        self.select_to(target, cx);
    }

    fn select_paragraph_down(
        &mut self,
        _: &SelectParagraphDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = text_motion::next_paragraph_end(&self.content, self.cursor());
        self.select_to(target, cx);
    }

    /// The start of the row the caret is on as drawn, which for a wrapped
    /// paragraph is not the start of the paragraph: ⌘← goes to the left
    /// edge of what you are looking at.
    fn visual_line_start(&self, offset: usize) -> usize {
        match self.point_for_offset(offset) {
            Some(here) => self.offset_for_point(point(px(0.), here.y)),
            None => self.row_bounds(offset).map(|(start, _)| start).unwrap_or(0),
        }
    }

    fn visual_line_end(&self, offset: usize) -> usize {
        let Some(here) = self.point_for_offset(offset) else {
            return self.row_bounds(offset).map(|(_, end)| end).unwrap_or(self.content.len());
        };
        let end = self.offset_for_point(point(px(f32::MAX / 4.), here.y));
        // A wrapped row ends at the space it wrapped on; landing after it
        // would put the caret at the start of the next row instead.
        let logical_end = self.row_bounds(offset).map(|(_, end)| end).unwrap_or(self.content.len());
        if end < logical_end && self.content[..end].ends_with(' ') {
            end - 1
        } else {
            end
        }
    }

    /// The offsets bounding the logical line containing `offset`.
    fn row_bounds(&self, offset: usize) -> Option<(usize, usize)> {
        self.layout
            .rows
            .iter()
            .find(|row| offset >= row.start && offset <= row.end())
            .map(|row| (row.start, row.end()))
    }

    // ---- editing ----

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor());
        self.delete_back_to(target, EditKind::Deleting, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor());
        self.delete_forward_to(target, EditKind::Deleting, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::previous_word_start(&self.content, self.cursor());
        self.delete_back_to(target, EditKind::Other, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = text_motion::next_word_end(&self.content, self.cursor());
        self.delete_forward_to(target, EditKind::Other, cx);
    }

    fn delete_to_line_start(&mut self, _: &DeleteToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.visual_line_start(self.cursor());
        self.delete_back_to(target, EditKind::Other, cx);
    }

    /// ⌃K and ⌘⌦. At the very end of a line there is nothing left on it, so
    /// it takes the line break instead, the way Cocoa's does.
    fn delete_to_line_end(&mut self, _: &DeleteToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let mut target = self.visual_line_end(cursor);
        if target == cursor {
            target = self.next_boundary(cursor);
        }
        self.delete_forward_to(target, EditKind::Other, cx);
    }

    /// Deletes the selection, or from `target` up to the caret when nothing
    /// is selected.
    fn delete_back_to(&mut self, target: usize, kind: EditKind, cx: &mut Context<Self>) {
        let range = match self.selected_range.is_empty() {
            true => target.min(self.cursor())..self.cursor(),
            false => self.selected_range.clone(),
        };
        self.edit(range, "", kind, cx);
    }

    fn delete_forward_to(&mut self, target: usize, kind: EditKind, cx: &mut Context<Self>) {
        let range = match self.selected_range.is_empty() {
            true => self.cursor()..target.max(self.cursor()),
            false => self.selected_range.clone(),
        };
        self.edit(range, "", kind, cx);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.marked_range.clone().unwrap_or_else(|| self.selected_range.clone());
        self.edit(range, "\n", EditKind::Typing, cx);
    }

    /// Every change to the text goes through here, which is what lets ⌘Z
    /// take any of them back.
    fn edit(&mut self, range: Range<usize>, new_text: &str, kind: EditKind, cx: &mut Context<Self>) {
        if self.read_only.is_some() {
            return;
        }
        let range = self.safe_range(range);
        if range.is_empty() && new_text.is_empty() {
            return;
        }
        // A letter that replaces a selection is the start of a new run, not
        // part of the one before.
        let kind = match self.selected_range.is_empty() {
            true => kind,
            false => EditKind::Other,
        };
        // A composition being committed was recorded when it began.
        if self.marked_range.is_none() {
            self.history.record(kind, &self.content, self.selected_range.clone(), self.cursor());
        }

        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        self.history.edited(kind, cursor);
        self.reveal = true;

        self.blink.wake(cx);
        cx.emit(AreaChanged);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only.is_some() {
            return;
        }
        if let Some((content, selection)) =
            self.history.undo(&self.content, self.selected_range.clone())
        {
            self.restore(content, selection, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only.is_some() {
            return;
        }
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
        self.goal_x = None;
        self.reveal = true;
        self.blink.wake(cx);
        cx.emit(AreaChanged);
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.goal_x = None;
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only.is_some() {
            return;
        }
        let Some(item) = cx.read_from_clipboard() else { return };

        // Text wins when the clipboard holds both. Copying from a rich editor
        // very often puts a rendered picture of the text on the clipboard
        // beside the text itself, and pasting a screenshot of the paragraph
        // you meant to quote is not what anybody asked for.
        if let Some(text) = item.text() {
            // Newlines are kept: this is the widget where they mean something.
            let range = self.selected_range.clone();
            self.edit(range, &text.replace("\r\n", "\n"), EditKind::Other, cx);
            return;
        }

        if let Some(image) = item.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        }) {
            cx.emit(ImagePasted(image));
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only.is_some() {
            return self.copy(&Copy, window, cx);
        }
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

    // ---- mouse ----

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A read-only area takes focus on mouse up, and only if the click
        // selected something; see `ReadOnly::return_focus`.
        if self.read_only.is_some() {
            window.prevent_default();
        }

        self.is_selecting = true;
        self.granularity = match event.click_count {
            0 | 1 => Granularity::Char,
            2 => Granularity::Word,
            _ => Granularity::Line,
        };

        if self.granularity == Granularity::Char {
            let offset = self.offset_for_window_point(event.position);
            self.anchor = offset..offset;
            if event.modifiers.shift {
                self.select_to(offset, cx);
            } else {
                self.move_to(offset, cx);
            }
            return;
        }

        // The letter under the pointer rather than the nearest gap between
        // two: clicking the right half of a word's last letter is still a
        // click on that word, not on the space after it.
        let offset = self.offset_under_window_point(event.position);
        let unit = self.unit_at(offset);
        self.anchor = unit.clone();
        self.selected_range = unit;
        self.selection_reversed = false;
        self.goal_x = None;
        self.blink.wake(cx);
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let was_selecting = std::mem::replace(&mut self.is_selecting, false);
        let Some(read_only) = &self.read_only else { return };
        if !was_selecting {
            return;
        }

        if !self.selected_range.is_empty() {
            window.focus(&self.focus_handle);
            return;
        }

        // A click that selected nothing: follow a link if it was on one, and
        // give focus back rather than holding it with nothing to copy.
        if event.click_count <= 1 {
            let offset = self.offset_under_window_point(event.position);
            if let Some((_, url)) =
                read_only.links.iter().find(|(range, _)| range.contains(&offset))
            {
                // Handing the URL to the browser is the safe default: the
                // reader deliberately has no way to run what it is given.
                cx.open_url(url);
            }
        }
        if self.focus_handle.is_focused(window)
            && let Some(back) = read_only.return_focus.clone()
        {
            window.focus(&back);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }

        if self.granularity == Granularity::Char {
            let offset = self.offset_for_window_point(event.position);
            self.select_to(offset, cx);
            return;
        }

        // By whole units, from the one the click landed on to the one the
        // pointer is over, whichever way round that is.
        let unit = self.unit_at(self.offset_under_window_point(event.position));
        let anchor = self.anchor.clone();
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

    /// The word or line at `offset`, depending on how many clicks made it.
    fn unit_at(&self, offset: usize) -> Range<usize> {
        match self.granularity {
            Granularity::Line => line_range(&self.content, offset),
            _ => word_range(&self.content, offset),
        }
    }

    // ---- selection bookkeeping ----

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        // Any horizontal or absolute move forgets the column being aimed for.
        self.goal_x = None;
        self.reveal = true;
        self.blink.wake(cx);
        cx.notify();
    }

    /// Like [`Self::move_to`], but keeps `goal_x` — for vertical movement,
    /// which is the only thing that should preserve it.
    fn move_keeping_goal(&mut self, offset: usize, cx: &mut Context<Self>) {
        let goal = self.goal_x;
        self.move_to(offset, cx);
        self.goal_x = goal;
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
        self.reveal = true;
        self.blink.wake(cx);
        cx.notify();
    }

    // ---- geometry ----

    /// Where a byte offset sits, relative to the top-left of the element.
    fn point_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let line_height = self.layout.line_height;

        for row in &self.layout.rows {
            if offset <= row.end() {
                let local = row.line.position_for_index(offset - row.start, line_height)?;
                return Some(point(local.x, row.top + local.y));
            }
        }
        // Past the end: the last row's own end.
        let row = self.layout.rows.last()?;
        let local = row.line.position_for_index(row.line.len(), line_height)?;
        Some(point(local.x, row.top + local.y))
    }

    /// The offset nearest a point relative to the top-left of the element.
    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        self.offset_at_point(position, true)
    }

    /// The offset of the character a point is over, rather than the gap
    /// nearest it — which is what a double click or a link needs to know.
    fn offset_under_window_point(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.layout.bounds else { return 0 };
        self.offset_at_point(position - bounds.origin, false)
    }

    fn offset_at_point(&self, position: Point<Pixels>, closest: bool) -> usize {
        let line_height = self.layout.line_height;
        let Some(rows) = (!self.layout.rows.is_empty()).then_some(&self.layout.rows) else {
            return 0;
        };

        for row in rows {
            let bottom = row.top + row.height(line_height);
            if position.y < bottom {
                let local = point(position.x, (position.y - row.top).max(px(0.)));
                let index = match closest {
                    true => row.line.closest_index_for_position(local, line_height),
                    false => row.line.index_for_position(local, line_height),
                }
                .unwrap_or_else(|index| index);
                return row.start + index.min(row.line.len());
            }
        }
        self.content.len()
    }

    /// The same, for a point in window coordinates.
    fn offset_for_window_point(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.layout.bounds else { return 0 };
        self.offset_for_point(position - bounds.origin)
    }

    // ---- offsets ----

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

    /// A range that is safe to slice `content` with. See the same helper on
    /// [`crate::text_input::TextInput`] for why this exists: a bad range here
    /// is an `abort()`, not a bad edit.
    fn safe_range(&self, range: Range<usize>) -> Range<usize> {
        let start = crate::text_input::floor_boundary(&self.content, range.start);
        let end = crate::text_input::floor_boundary(&self.content, range.end);
        start.min(end)..start.max(end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Grapheme-aware, so an emoji or a combining accent moves as one unit.
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

impl EntityInputHandler for TextArea {
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
        if self.read_only.is_some() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        // Committing a composition is still typing, but there is nothing to
        // record when the text it commits is already there.
        if self.marked_range.as_ref() == Some(&range)
            && self.content.get(range.clone()) == Some(new_text)
        {
            self.marked_range = None;
            cx.notify();
            return;
        }
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
        if self.read_only.is_some() {
            return;
        }
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
                self.cursor(),
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
        self.reveal = true;

        self.blink.wake(cx);
        cx.emit(AreaChanged);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.safe_range(self.range_from_utf16(&range_utf16));
        let start = self.point_for_offset(range.start)?;
        let end = self.point_for_offset(range.end)?;

        Some(Bounds::from_corners(
            bounds.origin + start,
            bounds.origin + point(end.x, end.y + self.layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.offset_for_window_point(point)))
    }
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The word at `offset`, or the run of spaces or punctuation if that is what
/// is there — the same unit a double click selects everywhere else on a Mac.
///
/// Word boundaries are Unicode's (UAX #29), so "don't" and "3.14" are one
/// word each and an emoji is one of its own.
fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let mut before: Option<Range<usize>> = None;

    for (start, segment) in text.split_word_bound_indices() {
        let range = start..start + segment.len();
        if range.contains(&offset) {
            // Past the end of a line the pointer is over the newline, and a
            // double click there means the last thing on the line, not the
            // line break.
            let is_break = |s: &str| s.chars().all(|c| c == '\n' || c == '\r');
            return match (is_break(segment), before) {
                (true, Some(before)) if !is_break(&text[before.clone()]) => before,
                (true, _) => offset..offset,
                (false, _) => range,
            };
        }
        before = Some(range);
    }
    // At the very end: the last word, if the text does not end in a newline.
    match before {
        Some(range) if !text[range.clone()].ends_with('\n') => range,
        _ => offset..offset,
    }
}

/// The logical line at `offset`, with the newline that ends it — what a
/// triple click selects, so that deleting or copying it takes the whole line.
fn line_range(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map(|ix| ix + 1).unwrap_or(0);
    let end = text[offset..].find('\n').map(|ix| offset + ix + 1).unwrap_or(text.len());
    start..end
}

/// Builds the shaped runs, with the selection expressed as a background colour
/// and the IME's composition underlined.
///
/// Doing selection this way rather than as painted quads is what makes a
/// selection across many wrapped rows free: `paint_background` already knows
/// how to follow the wrapping.
fn runs_for(
    text: &str,
    base: TextRun,
    selection: Option<Range<usize>>,
    marked: Option<Range<usize>>,
    selection_color: gpui::Hsla,
    colors: &[(Range<usize>, Hsla)],
) -> Vec<TextRun> {
    let mut edges = vec![0usize, text.len()];
    let ranges = [selection.clone(), marked.clone()]
        .into_iter()
        .flatten()
        .chain(colors.iter().map(|(range, _)| range.clone()));
    for range in ranges {
        // On a character boundary, or shaping slices a character in two.
        edges.push(crate::text_input::floor_boundary(text, range.start.min(text.len())));
        edges.push(crate::text_input::floor_boundary(text, range.end.min(text.len())));
    }
    edges.sort_unstable();
    edges.dedup();

    let mut runs = Vec::with_capacity(edges.len());
    for pair in edges.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if end <= start {
            continue;
        }

        let selected = selection.as_ref().is_some_and(|r| start >= r.start && end <= r.end);
        let composing = marked.as_ref().is_some_and(|r| start >= r.start && end <= r.end);
        let color = colors
            .iter()
            .find(|(r, _)| start >= r.start && end <= r.end)
            .map(|(_, color)| *color)
            .unwrap_or(base.color);

        runs.push(TextRun {
            len: end - start,
            color,
            background_color: selected.then_some(selection_color),
            underline: composing.then(|| UnderlineStyle {
                color: Some(base.color),
                thickness: px(1.),
                wavy: false,
            }),
            ..base.clone()
        });
    }
    runs
}

/// Draws the content, its selection and the caret, and installs the IME bridge.
struct AreaElement {
    area: Entity<TextArea>,
    theme: Theme,
}

struct Prepaint {
    caret: Option<PaintQuad>,
}

impl IntoElement for AreaElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for AreaElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

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
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let area = self.area.clone();
        let theme = self.theme.clone();

        // Measured rather than fixed: the height depends on how the text wraps,
        // which depends on the width the parent gives us.
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, cx| {
                let wrap_width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });

                // Read what shaping needs, then shape, then store. Split in
                // three because shaping borrows the window while `update`
                // borrows the app, and the two cannot overlap.
                let request = area.read(cx).shape_request();
                let layout = shape(request, wrap_width, &theme, window);
                let height = layout.height();

                area.update(cx, |area, _| {
                    // The bounds come from prepaint and are not shaping's to
                    // overwrite.
                    let bounds = area.layout.bounds;
                    area.layout = layout;
                    area.layout.bounds = bounds;
                });

                size(wrap_width.unwrap_or(px(0.)), height)
            },
        );

        (layout_id, ())
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
        let accent = self.theme.accent;

        // Where the caret now is, if a keystroke moved it and there is a box
        // to scroll to it.
        let reveal = self.area.update(cx, |area, _| {
            if !area.reveal {
                return None;
            }
            let found = area.follow.clone().zip(area.point_for_offset(area.cursor()));
            if found.is_none() {
                area.reveal = false;
            }
            found.map(|(follow, at)| (follow, bounds.origin.y + at.y, area.layout.line_height))
        });
        if let Some((follow, top, line_height)) = reveal {
            let (view, offset, max) = (follow.bounds(), follow.offset(), follow.max_offset().height);
            match offset_revealing(view, offset, max, top, line_height) {
                // Too late to move what this frame has already placed, so the
                // next one draws at the new offset — and checks again, since
                // a window still settling its layout can take it back.
                Some(offset) => {
                    follow.set_offset(offset);
                    window.request_animation_frame();
                }
                // In view, or as near as the box can scroll.
                None => self.area.update(cx, |area, _| area.reveal = false),
            }
        }

        let caret = self.area.update(cx, |area, _| {
            area.layout.bounds = Some(bounds);
            let ink = area.style.caret.unwrap_or(accent);

            // With the placeholder showing there is no content to sit in, so
            // the caret goes at the very start rather than at a placeholder
            // offset that means nothing.
            let offset = if area.content.is_empty() { 0 } else { area.cursor() };
            if area.read_only.is_some() {
                return None;
            }

            area.point_for_offset(offset).map(|position| {
                fill(
                    Bounds::new(
                        bounds.origin + position,
                        size(CARET_WIDTH, area.layout.line_height),
                    ),
                    ink,
                )
            })
        });

        Prepaint { caret }
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
        let focus_handle = self.area.read(cx).focus_handle.clone();
        // No input handler on text that cannot be typed into: with one, every
        // letter pressed while it had focus would be swallowed here instead of
        // reaching the window's own shortcuts.
        if self.area.read(cx).read_only.is_none() {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.area.clone()),
                cx,
            );
        }

        let (rows, line_height) = self.area.update(cx, |area, _| {
            let line_height = area.layout.line_height;
            let placements: Vec<(Point<Pixels>, WrappedLine)> = area
                .layout
                .rows
                .iter()
                .map(|row| (bounds.origin + point(px(0.), row.top), row.line.clone()))
                .collect();
            (placements, line_height)
        });

        for (origin, line) in &rows {
            line.paint_background(*origin, line_height, TextAlign::Left, None, window, cx)
                .ok();
        }
        for (origin, line) in &rows {
            line.paint(*origin, line_height, TextAlign::Left, None, window, cx).ok();
        }

        let focused = focus_handle.is_focused(window);
        // Paint is where the blink hears about focus: the area is built before
        // there is a window, so there is nowhere earlier to ask.
        let blink_on = self.area.update(cx, |area, cx| {
            area.blink.set_focused(focused, cx);
            area.blink.visible()
        });

        if focused
            && blink_on
            && let Some(caret) = prepaint.caret.take()
        {
            window.paint_quad(caret);
        }
    }
}

/// Everything shaping needs, copied out of the entity so the shaping itself
/// borrows nothing.
struct ShapeRequest {
    text: SharedString,
    showing_placeholder: bool,
    selection: Option<Range<usize>>,
    marked: Option<Range<usize>>,
    style: AreaStyle,
    colors: Vec<(Range<usize>, Hsla)>,
}

impl TextArea {
    fn shape_request(&self) -> ShapeRequest {
        let showing_placeholder = self.content.is_empty();

        ShapeRequest {
            text: if showing_placeholder {
                self.placeholder.clone()
            } else {
                self.content.clone()
            },
            showing_placeholder,
            selection: (!showing_placeholder && !self.selected_range.is_empty())
                .then(|| self.selected_range.clone()),
            marked: self.marked_range.clone().filter(|_| !showing_placeholder),
            style: self.style.clone(),
            colors: match (&self.read_only, showing_placeholder) {
                (Some(read_only), false) => read_only.colors.clone(),
                _ => Vec::new(),
            },
        }
    }
}

/// Shapes the content into rows.
/// The scroll offset that shows a caret whose top is at `top` (in window
/// coordinates) with a line of room on either side, or `None` when it is
/// already in view.
///
/// `view` is the scrolling box, `offset` its current scroll (zero or less, as
/// gpui counts it) and `max` how far it can scroll. Moves only as far as it
/// must, the way every editor scrolls while you type: down until the caret's
/// line clears the bottom, or up until it clears the top.
fn offset_revealing(
    view: Bounds<Pixels>,
    current: Point<Pixels>,
    max: Pixels,
    top: Pixels,
    line_height: Pixels,
) -> Option<Point<Pixels>> {
    let mut offset = current;
    let bottom = top + line_height;
    if bottom + line_height > view.bottom() {
        offset.y -= bottom + line_height - view.bottom();
    } else if top - line_height < view.top() {
        offset.y += view.top() - (top - line_height);
    } else {
        return None;
    }
    offset.y = offset.y.clamp(-max, px(0.));
    (offset != current).then_some(offset)
}

fn shape(
    request: ShapeRequest,
    wrap_width: Option<Pixels>,
    theme: &Theme,
    window: &mut Window,
) -> Layout {
    let mut style = window.text_style();
    if let Some(family) = &request.style.font_family {
        style.font_family = family.clone();
    }
    if let Some(size) = request.style.font_size {
        style.font_size = size.into();
    }
    if let Some(weight) = request.style.font_weight {
        style.font_weight = weight;
    }

    let font_size = style.font_size.to_pixels(window.rem_size());
    // Proportional to the size rather than the window's leading: a bigger face
    // with the window's line height is a paragraph that overlaps itself.
    let line_height = match (request.style.line_height, request.style.font_size) {
        (Some(line_height), _) => line_height,
        (None, Some(_)) => font_size * 1.45,
        (None, None) => window.line_height(),
    };

    let base = TextRun {
        len: request.text.len(),
        font: style.font(),
        color: match (request.showing_placeholder, request.style.ink) {
            (true, _) => theme.text_faint,
            (false, Some(ink)) => ink,
            (false, None) => style.color,
        },
        background_color: None,
        underline: None,
        strikethrough: None,
    };

    let runs = runs_for(
        &request.text,
        base,
        request.selection,
        request.marked,
        request.style.selection.unwrap_or(theme.text_selection),
        &request.colors,
    );

    let Ok(lines) =
        window.text_system().shape_text(request.text, font_size, &runs, wrap_width, None)
    else {
        return Layout { line_height, ..Layout::default() };
    };

    let mut rows = Vec::with_capacity(lines.len());
    let mut start = 0usize;
    let mut top = px(0.);

    for line in lines {
        let count = line.wrap_boundaries().len() + 1;
        let length = line.len();

        rows.push(Row { start, line, top, rows: count });
        top += line_height * count as f32;
        // Past the line's own bytes and the newline that ended it.
        start += length + 1;
    }

    Layout { rows, bounds: None, line_height }
}

impl Render for TextArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let context = if self.read_only.is_some() { "ReadOnlyText" } else { "TextArea" };

        gpui::div()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::document_start))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_to_line_start))
            .on_action(cx.listener(Self::select_to_line_end))
            .on_action(cx.listener(Self::select_to_document_start))
            .on_action(cx.listener(Self::select_to_document_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::paragraph_up))
            .on_action(cx.listener(Self::paragraph_down))
            .on_action(cx.listener(Self::select_paragraph_up))
            .on_action(cx.listener(Self::select_paragraph_down))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            // At least as tall as the box it is in, so a click anywhere in
            // the box lands in it, and taller when the text is: a box that
            // scrolls only knows how far from the height of what is in it,
            // and at `size_full` that was never more than the box itself.
            .w_full()
            .min_h_full()
            // No colour of its own: the text takes whatever the element it
            // sits in is using, which is what lets the composer put a white
            // page under it without the area insisting on the theme's ink.
            .child(AreaElement { area: cx.entity(), theme })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run splitting is the part that is pure enough to test without a
    /// window, and the part where an off-by-one silently mis-colours a
    /// selection.
    fn base_run(len: usize) -> TextRun {
        TextRun {
            len,
            font: gpui::Font {
                family: "test".into(),
                features: Default::default(),
                fallbacks: None,
                weight: Default::default(),
                style: Default::default(),
            },
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    fn highlight() -> gpui::Hsla {
        gpui::blue()
    }

    #[test]
    fn a_run_with_no_selection_is_a_single_span() {
        let text = "hello world";
        let runs = runs_for(text, base_run(text.len()), None, None, highlight(), &[]);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, text.len());
        assert!(runs[0].background_color.is_none());
    }

    #[test]
    fn a_selection_splits_the_run_and_only_it_is_highlighted() {
        let text = "hello world";
        let runs = runs_for(text, base_run(text.len()), Some(6..11), None, highlight(), &[]);

        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());

        let highlighted: usize =
            runs.iter().filter(|r| r.background_color.is_some()).map(|r| r.len).sum();
        assert_eq!(highlighted, 5, "only \"world\" should be highlighted");
    }

    #[test]
    fn runs_always_cover_the_whole_text_exactly_once() {
        let text = "one two three four";
        for selection in [0..0, 0..4, 4..8, 8..text.len(), 0..text.len()] {
            let runs = runs_for(
                text,
                base_run(text.len()),
                (!selection.is_empty()).then_some(selection.clone()),
                None,
                highlight(),
                &[],
            );
            assert_eq!(
                runs.iter().map(|r| r.len).sum::<usize>(),
                text.len(),
                "selection {selection:?} lost or duplicated bytes",
            );
        }
    }

    #[test]
    fn a_composition_is_underlined_without_being_highlighted() {
        let text = "abcdef";
        let runs = runs_for(text, base_run(text.len()), None, Some(2..4), highlight(), &[]);

        let underlined: usize =
            runs.iter().filter(|r| r.underline.is_some()).map(|r| r.len).sum();
        assert_eq!(underlined, 2);
        assert!(runs.iter().all(|r| r.background_color.is_none()));
    }

    #[test]
    fn a_selection_overlapping_a_composition_produces_valid_runs() {
        let text = "abcdefghij";
        let runs = runs_for(text, base_run(text.len()), Some(2..8), Some(4..6), highlight(), &[]);

        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        assert!(runs.iter().all(|r| r.len > 0), "an empty run would confuse shaping");
    }

    #[test]
    fn ranges_past_the_end_are_clamped_rather_than_panicking() {
        let text = "short";
        let runs = runs_for(text, base_run(text.len()), Some(2..999), None, highlight(), &[]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    }

    #[test]
    fn a_double_click_selects_the_word_under_it() {
        let text = "reply to don't panic";
        assert_eq!(&text[word_range(text, 1)], "reply");
        // The last letter of a word is still that word.
        assert_eq!(&text[word_range(text, 4)], "reply");
        assert_eq!(&text[word_range(text, 5)], " ");
        assert_eq!(&text[word_range(text, 10)], "don't");
        assert_eq!(&text[word_range(text, text.len())], "panic");
    }

    #[test]
    fn a_double_click_past_the_end_of_a_line_takes_its_last_word() {
        let text = "first line\nsecond";
        // Offset 10 is the newline, which is where a click past "line" lands.
        assert_eq!(&text[word_range(text, 10)], "line");
        assert_eq!(&text[word_range(text, 12)], "second");
        assert_eq!(word_range("a\n\nb", 2), 2..2, "an empty line has no word");
    }

    fn a_box(top: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(top)), size(px(400.), px(height)))
    }

    #[test]
    fn a_caret_in_view_scrolls_nothing() {
        let view = a_box(100., 300.);
        assert_eq!(offset_revealing(view, point(px(0.), px(0.)), px(500.), px(200.), px(18.)), None);
    }

    #[test]
    fn a_caret_past_the_bottom_scrolls_down_just_enough_to_show_it() {
        // The box ends at 400; a caret line from 420 to 438 needs another
        // line of room below it, so the page moves up by 438 + 18 - 400.
        let view = a_box(100., 300.);
        let offset = offset_revealing(view, point(px(0.), px(0.)), px(500.), px(420.), px(18.));
        assert_eq!(offset, Some(point(px(0.), px(-56.))));
    }

    #[test]
    fn a_caret_above_the_top_scrolls_back_up_to_it() {
        let view = a_box(100., 300.);
        let offset = offset_revealing(view, point(px(0.), px(-200.)), px(500.), px(60.), px(18.));
        assert_eq!(offset, Some(point(px(0.), px(-142.))));
    }

    #[test]
    fn scrolling_to_the_caret_stays_within_what_the_box_can_scroll() {
        let view = a_box(100., 300.);
        // Past the end of what can scroll: held at the end.
        let down = offset_revealing(view, point(px(0.), px(-90.)), px(100.), px(900.), px(18.));
        assert_eq!(down, Some(point(px(0.), px(-100.))));
        // Already at the end: nothing to do.
        assert_eq!(offset_revealing(view, point(px(0.), px(-100.)), px(100.), px(900.), px(18.)), None);
        // Above the top with the page already at the top: nothing to do.
        assert_eq!(offset_revealing(view, point(px(0.), px(0.)), px(100.), px(90.), px(18.)), None);
    }

    #[test]
    fn a_triple_click_selects_the_line_and_its_newline() {
        let text = "one\ntwo three\nfour";
        assert_eq!(&text[line_range(text, 6)], "two three\n");
        assert_eq!(&text[line_range(text, 0)], "one\n");
        assert_eq!(&text[line_range(text, text.len())], "four");
    }

    #[test]
    fn coloured_ranges_recolour_only_their_own_bytes() {
        let text = "> quoted\nreply";
        let runs = runs_for(text, base_run(text.len()), None, None, highlight(), &[(0..8, gpui::red())]);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].len, runs[0].color), (8, gpui::red()));
        assert_eq!((runs[1].len, runs[1].color), (text.len() - 8, gpui::black()));
    }
}
