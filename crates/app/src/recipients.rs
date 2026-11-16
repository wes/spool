//! The To, Cc and Bcc fields: a row of addresses you can take one out of.
//!
//! These were a single [`TextInput`] holding a comma-separated list, which
//! round-tripped correctly through `address::decode_list` and was unpleasant
//! in exactly two ways. Removing the second of four recipients meant selecting
//! the right run of characters and no more; and an address with a typo in it
//! looked like every other address until the send was refused.
//!
//! So each committed address becomes a pill, and one editing token stays at
//! the end where the caret is.
//!
//! # Why this is built around a `TextInput` rather than beside one
//!
//! The obvious shape for this is a third sibling to [`TextInput`] and
//! `TextArea`, owning its own text. It is not what this does, and the reason
//! is IME. A field that composes its own text has to implement
//! `EntityInputHandler` — marked ranges, the character palette, the bounds a
//! candidate window is drawn against — and getting that subtly wrong is how an
//! app ends up unable to address mail to anyone whose name is typed in
//! Japanese.
//!
//! The token being typed is the only part of this widget that is text, so it
//! is the widget that already handles text. What is added around it is a list
//! of addresses, a selection, and the two keys that cross between them.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Window,
    div, prelude::*, px,
};
use spool_core::{Address, address};

use crate::text_input::{Edge, InputChanged, TextInput};
use crate::theme::{Theme, ThemeAccess};

/// One committed recipient.
///
/// The text is kept alongside the parsed address rather than thrown away,
/// because a pill that does not parse still has to show what was written and
/// still has to be editable. Dropping it would delete what somebody typed on
/// the grounds that they had not finished typing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    text: String,
    address: Option<Address>,
}

impl Pill {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        // One part in, one address out, or nothing: `decode_list` drops what
        // it cannot read, and here that is the answer to whether it is valid.
        let address = address::decode_list(&text).into_iter().next();
        Self { text, address }
    }

    pub fn label(&self) -> SharedString {
        match &self.address {
            // The name when there is one, since the address is what the pill
            // is *for* and the name is how it is recognised.
            Some(a) if !a.name.is_empty() => a.name.clone().into(),
            Some(a) => a.addr.clone().into(),
            None => self.text.clone().into(),
        }
    }

    /// Whether this is an address that could actually be delivered to.
    ///
    /// A stricter question than "did it parse". `address::decode_list` is
    /// deliberately lenient — it is used on headers that real servers wrote,
    /// where refusing to read something is worse than reading it oddly — so
    /// `ana@` comes back as an address. Here the person is still typing, and
    /// saying so is the whole point of the red pill.
    ///
    /// Note that the lenient parse is what is *kept*: an address this rejects
    /// is still carried into the draft, still saved, and still shown. It is
    /// marked, not discarded.
    pub fn is_valid(&self) -> bool {
        self.address.as_ref().is_some_and(|a| deliverable(&a.addr))
    }
}

/// A local part, an `@`, and a domain with a dot in it.
///
/// The same test the completion list uses to decide someone has finished
/// typing a name, which is the right place for the two to agree: the moment
/// suggestions stop is the moment the pill should stop looking wrong.
fn deliverable(addr: &str) -> bool {
    match addr.trim().rsplit_once('@') {
        Some((local, domain)) => !local.trim().is_empty() && domain.contains('.'),
        None => false,
    }
}

/// Emitted when the set of recipients changes, so the composer can save the
/// draft and ask for suggestions again.
pub struct RecipientsChanged;

pub struct RecipientField {
    pills: Vec<Pill>,
    /// The pill the keyboard is on, if the caret has walked back into them.
    ///
    /// Not a text selection: a pill is selected whole or not at all, which is
    /// the entire point of it being a pill.
    selected: Option<usize>,
    /// The token being typed, which is the only text here.
    input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RecipientsChanged> for RecipientField {}

impl RecipientField {
    pub fn new(placeholder: &'static str, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(placeholder, cx));

        let subscriptions = vec![
            cx.subscribe(&input, |this, _, event: &InputChanged, cx| {
                this.on_typed(event.0.to_string(), cx);
            }),
            cx.subscribe(&input, |this, _, event: &Edge, cx| match event {
                Edge::BackspaceAtStart => this.backspace_into_pills(cx),
                Edge::LeftAtStart => this.select_left(cx),
                Edge::RightAtStart => this.select_right(cx),
            }),
        ];

        Self { pills: Vec::new(), selected: None, input, _subscriptions: subscriptions }
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle.clone()
    }

    /// Every address in the field, including one still being typed.
    ///
    /// The token counts: someone who fills in a single address and presses
    /// send has never typed the comma that would have committed it, and a
    /// message refused for having no recipients while a recipient is on
    /// screen is indefensible.
    pub fn addresses(&self, cx: &App) -> Vec<Address> {
        self.pills
            .iter()
            .filter_map(|pill| pill.address.clone())
            .chain(address::decode_list(self.input.read(cx).text()))
            .collect()
    }

    /// The pills, for drawing and for asking what is wrong.
    pub fn pills(&self) -> &[Pill] {
        &self.pills
    }

    /// What is being typed right now, which is what completion completes.
    pub fn token(&self, cx: &App) -> String {
        self.input.read(cx).text().trim().to_string()
    }

    /// Replaces everything, for opening a draft.
    pub fn set_addresses(&mut self, addresses: &[Address], cx: &mut Context<Self>) {
        self.pills = addresses.iter().map(|a| Pill::new(a.encoded())).collect();
        self.selected = None;
        self.input.update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.notify();
    }

    /// Takes a chosen suggestion, as a pill rather than as text.
    pub fn accept(&mut self, address: &Address, cx: &mut Context<Self>) {
        self.pills.push(Pill::new(address.encoded()));
        self.selected = None;
        // Cleared rather than filled with a trailing comma: the comma existed
        // to separate this address from the next one, and a pill does that.
        self.input.update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.emit(RecipientsChanged);
        cx.notify();
    }

    /// Commits whatever is in the token, for the moment focus leaves.
    pub fn commit(&mut self, cx: &mut Context<Self>) {
        let token = self.input.read(cx).text().trim().to_string();
        if token.is_empty() {
            return;
        }
        self.pills.push(Pill::new(token));
        self.input.update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.emit(RecipientsChanged);
        cx.notify();
    }

    /// Everything typed goes through here.
    ///
    /// A comma is what commits an address, and that one rule covers both ways
    /// addresses arrive: typed one at a time, or pasted as a whole `To:`
    /// header. What is left after the last comma stays in the token, because
    /// it is what the person is still in the middle of.
    fn on_typed(&mut self, text: String, cx: &mut Context<Self>) {
        // Typing is also how a pill stops being selected: the caret is back
        // in the token, and leaving a pill lit would say otherwise.
        self.selected = None;

        let Some(last) = text.rfind(',') else {
            cx.emit(RecipientsChanged);
            cx.notify();
            return;
        };

        let (committed, rest) = text.split_at(last + 1);
        // `split_list` and not `split(',')`: a display name may contain a
        // comma, and `"Ruiz, Ana" <ana@x>` is one recipient rather than two.
        let added: Vec<Pill> = address::split_list(committed).map(Pill::new).collect();
        let rest = rest.to_string();

        if added.is_empty() && rest.trim().is_empty() {
            // A lone comma, or a run of them. Nothing to commit and nothing
            // worth keeping, so the field simply swallows it.
            self.input.update(cx, |input, cx| input.set_content(String::new(), cx));
            return;
        }

        self.pills.extend(added);
        self.input.update(cx, |input, cx| input.set_content(rest, cx));
        cx.emit(RecipientsChanged);
        cx.notify();
    }

    /// Backspace, with the caret already at the front of the token.
    fn backspace_into_pills(&mut self, cx: &mut Context<Self>) {
        let doomed = match self.selected {
            Some(index) => index,
            None => match self.pills.is_empty() {
                true => return,
                false => self.pills.len() - 1,
            },
        };

        self.pills.remove(doomed);
        // The caret lands where the pill was, so a run of backspaces walks
        // backwards through the list rather than stopping after one.
        self.selected = doomed.checked_sub(1).filter(|_| !self.pills.is_empty());
        cx.emit(RecipientsChanged);
        cx.notify();
    }

    fn select_left(&mut self, cx: &mut Context<Self>) {
        if self.pills.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None => self.pills.len() - 1,
            Some(index) => index.saturating_sub(1),
        });
        cx.notify();
    }

    /// The way back out of the pills, towards the token.
    ///
    /// Only ever reached with an empty token — see [`Edge::RightAtStart`] —
    /// so there is no caret movement being swallowed here.
    fn select_right(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.selected else { return };
        self.selected = (index + 1 < self.pills.len()).then_some(index + 1);
        cx.notify();
    }

    fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.pills.len() {
            return;
        }
        self.pills.remove(index);
        self.selected = None;
        cx.emit(RecipientsChanged);
        cx.notify();
    }
}

impl Focusable for RecipientField {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle.clone()
    }
}

impl Render for RecipientField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let pills: Vec<_> = self
            .pills
            .iter()
            .enumerate()
            .map(|(index, pill)| {
                render_pill(index, pill, self.selected == Some(index), &theme, cx)
            })
            .collect();

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(pills)
            // The token takes the rest of the line, and never less than enough
            // to see what is being typed: without a floor it is squeezed to
            // nothing by a row of pills that has just filled the width.
            .child(div().flex_1().min_w(px(120.)).child(self.input.clone()))
    }
}

/// One address, as something you can see the shape of and take out whole.
fn render_pill(
    index: usize,
    pill: &Pill,
    selected: bool,
    theme: &Theme,
    cx: &mut Context<RecipientField>,
) -> gpui::Stateful<gpui::Div> {
    let valid = pill.is_valid();

    div()
        .id(("recipient", index))
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .pl_2()
        .pr_1()
        .rounded_full()
        .border_1()
        .text_size(theme.small_text_size)
        .cursor_pointer()
        // An address that does not parse says so here, while it is being
        // written, rather than at the moment the send is refused -- which is
        // after the window has been closed and the thought has been let go.
        .when(!valid, |s| s.border_color(theme.red).text_color(theme.red))
        .when(valid && selected, |s| {
            s.border_color(theme.accent).bg(theme.selected).text_color(theme.text)
        })
        .when(valid && !selected, |s| {
            s.border_color(theme.border).bg(theme.panel).text_color(theme.text)
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            this.selected = Some(index);
            cx.notify();
        }))
        .child(div().child(pill.label()))
        .child(
            div()
                .id(("remove", index))
                .px_1()
                .rounded_full()
                .text_color(theme.text_faint)
                .hover(|s| s.text_color(theme.text))
                .on_click(cx.listener(move |this, _, _, cx| this.remove(index, cx)))
                .child("\u{00D7}"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pill_knows_whether_its_address_is_one() {
        assert!(Pill::new("ana@example.com").is_valid());
        assert!(Pill::new("Ana Ruiz <ana@example.com>").is_valid());

        // The shapes a half-typed address has. `decode_list` is lenient
        // enough to read the first two as addresses, which is right for a
        // header a server wrote and wrong for something being typed.
        for half in ["ana@", "ana@example", "Ana Ruiz", ""] {
            assert!(!Pill::new(half).is_valid(), "{half:?} is not deliverable");
        }

        // Marked, not discarded: what was typed is still there to fix.
        assert_eq!(Pill::new("ana@").label(), "ana@");
    }

    #[test]
    fn a_pill_reads_as_the_person_not_the_address() {
        assert_eq!(Pill::new("Ana Ruiz <ana@example.com>").label(), "Ana Ruiz");
        // Nothing else to call them by.
        assert_eq!(Pill::new("ana@example.com").label(), "ana@example.com");
    }

    #[test]
    fn an_accepted_suggestion_survives_becoming_a_pill() {
        // The round trip that matters, and the one the old comma-separated
        // field had to quote its way through: a pill is built from
        // `Address::encoded`, and a name containing a comma must come back as
        // one person rather than two.
        let awkward = Address::new("Ruiz, Ana", "ana@example.com");
        let pill = Pill::new(awkward.encoded());

        assert!(pill.is_valid());
        assert_eq!(pill.label(), "Ruiz, Ana");
        assert_eq!(pill.address.as_ref().unwrap().addr, "ana@example.com");
        assert_eq!(pill.address.as_ref().unwrap().name, "Ruiz, Ana");
    }

    /// The splitting [`RecipientField::on_typed`] does, without a window.
    fn commit(text: &str) -> (Vec<String>, String) {
        let Some(last) = text.rfind(',') else {
            return (Vec::new(), text.to_string());
        };
        let (committed, rest) = text.split_at(last + 1);
        (
            address::split_list(committed).map(str::to_string).collect(),
            rest.to_string(),
        )
    }

    #[test]
    fn a_comma_is_what_turns_a_token_into_a_pill() {
        let (pills, rest) = commit("ana@example.com,");
        assert_eq!(pills, ["ana@example.com"]);
        assert_eq!(rest, "", "and the caret starts the next one");

        // Still typing: nothing is committed until the comma.
        let (pills, rest) = commit("ana@exa");
        assert!(pills.is_empty());
        assert_eq!(rest, "ana@exa");
    }

    #[test]
    fn pasting_a_raw_header_splits_into_pills() {
        let (pills, rest) = commit("Ana <a@x.com>, Bo <b@y.com>, c@z.com");
        assert_eq!(pills, ["Ana <a@x.com>", "Bo <b@y.com>"]);
        // The last one has no comma after it, so it stays editable rather
        // than being committed on the strength of a guess.
        assert_eq!(rest, " c@z.com");
    }

    #[test]
    fn a_comma_inside_a_name_does_not_split_a_person_in_two() {
        // The case that made the old comma-separated field quietly wrong.
        let (pills, rest) = commit(r#""Ruiz, Ana" <ana@example.com>,"#);
        assert_eq!(pills, [r#""Ruiz, Ana" <ana@example.com>"#]);
        assert_eq!(rest, "");
        assert!(Pill::new(&pills[0]).is_valid());
    }
}
