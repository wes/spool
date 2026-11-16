//! Every user-visible operation is an action.
//!
//! This is the Zed model, and it is the reason the command palette is three
//! lines of code rather than a parallel implementation of the menu: keystrokes,
//! palette entries and click handlers all dispatch the same action, so there is
//! exactly one path from intent to behaviour.

use gpui::{App, KeyBinding, actions};

actions!(
    mailbox,
    [
        Quit,
        /// Opens the command palette.
        ToggleCommandPalette,
        /// Moves focus to the search field.
        FocusSearch,
        /// Dismisses the palette, clears search, or returns focus to the list.
        Cancel,
        /// Runs the highlighted palette command, or opens the selected message.
        Confirm,
        /// Runs an incremental sync of the selected account.
        Sync,
        /// Checks the inboxes for new mail, and nothing else.
        GetNewMail,
        /// Fetches the next batch of mail older than anything stored for the
        /// folder being looked at.
        LoadOlder,
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        PageDown,
        PageUp,
        /// Marks the selected message read and shows it in the reader.
        OpenSelected,
        /// Writes every attachment on the open message to Downloads and
        /// reveals them.
        ///
        /// An action rather than only a click target on a chip, which is what
        /// they were. Everything else in this app can be reached from the
        /// keyboard and the palette, and an attachment you can only get at by
        /// finding a small rectangle with the pointer is one you cannot get at
        /// when the pointer does not find it.
        SaveAttachments,
        /// Shows the open message's first attachment in Quick Look.
        PreviewAttachment,
        ToggleRead,
        /// Marks the selection read, whatever it was.
        MarkRead,
        /// Marks it unread, and stops the reader marking it read again.
        MarkUnread,
        ToggleFlag,
        Archive,
        DeleteMessage,
        /// Opens the palette on a list of folders to file the selection into.
        MoveToFolder,
        /// Files the selection into the account's Junk folder.
        MarkAsSpam,
        /// Takes it back out again, into the Inbox.
        NotSpam,
        /// Leaves the mailing list the open message came from.
        Unsubscribe,
        MarkAllRead,
        NextMailbox,
        /// Opens the About window.
        AboutSpool,
        /// Hands the keyboard to the sidebar, on the mailbox being shown.
        FocusSidebar,
        /// Hands the keyboard back to the message list.
        FocusMessageList,
        /// The next row of the sidebar, opened.
        SidebarNext,
        /// The row above, opened.
        SidebarPrev,
        /// Unfolds a row, or steps into an open one.
        SidebarExpand,
        /// Folds a row, or steps out to the one it sits under.
        SidebarCollapse,
        PrevMailbox,
        /// Cycles the list between all / unread / flagged.
        CycleFilter,
        /// Switches the reader between one message and the whole conversation.
        ToggleThreadView,
        /// Switches the reader between the formatted body and the plain-text
        /// alternative.
        ToggleBodySource,
        /// Switches the message list between the slim and detail shapes.
        ToggleDensity,
        /// Switches the message list between conversations and messages.
        ToggleConversations,
        ToggleSidebar,
        /// Shows or hides the status bar and its performance readout.
        ToggleStatusBar,
        /// Shows or hides the sidebar's unified "All Mail" row.
        ToggleUnifiedInbox,
        /// Opens the settings pane.
        OpenSettings,
        /// Looks for a newer version of Spool.
        CheckForUpdates,
        /// Makes Spool the app that opens `mailto:` links.
        MakeDefaultMailApp,
        /// Copies the selected message's sender to the clipboard.
        CopySender,
        /// Opens a blank message.
        Compose,
        /// Reopens the draft that was last put down.
        ///
        /// Its own action rather than something `Compose` does on the way
        /// past. "New Message" that hands back the reply you abandoned an hour
        /// ago is a surprise, and a surprise on the one command that has to be
        /// predictable.
        ResumeDraft,
        /// Replies to the sender.
        Reply,
        /// Replies to everyone on the message.
        ReplyAll,
        Forward,
        /// Sends the open draft.
        SendMessage,
        /// Throws the open draft away.
        DiscardDraft,
        /// Moves to the next field in the composer.
        NextField,
        PrevField,
        /// Scales the text in a composer's body.
        IncreaseComposeFont,
        DecreaseComposeFont,
        ResetComposeFont,
        /// Walks the recipient suggestions under a composer's To or Cc field.
        CompletionNext,
        CompletionPrev,
        /// Puts the highlighted suggestion into the field.
        AcceptCompletion,
        /// Selects every message in the list.
        SelectAllMessages,
        /// Extends the selection by one row.
        ExtendNext,
        ExtendPrev,
        /// Puts the last archive, move or trash back where it came from.
        Undo,
        /// Creates a folder on the server.
        NewMailbox,
        /// Renames the selected folder, which is also how it is moved.
        RenameMailbox,
        /// Deletes the selected folder and everything in it.
        DeleteMailbox,
        /// Opens Clients, on the sender of the message being read.
        OpenClients,
        /// Opens Clients on the board.
        OpenClientBoard,
        /// In Clients: the directory of people.
        ShowClientDirectory,
        /// In Clients: the sale, as columns.
        ShowClientBoard,
        /// In Clients: somebody typed in by hand.
        NewClient,
        /// In Clients: people from a CSV, vCard or JSON file.
        ImportContacts,
        /// In Clients: the endpoints signups are pulled from.
        ManageSources,
        /// In Clients: the chosen client one column left on the board.
        StageLeft,
        /// And one column right, which from off the board is the first.
        StageRight,
        /// In Clients: saves the note being written.
        SaveNote,
    ]
);

/// The default keymap.
///
/// Two contexts: `Spool` is the whole window, `MessageList` is the list when
/// it holds focus. Single-letter bindings live only in `MessageList` so that
/// typing `f` into the search field searches for an `f` instead of flagging
/// something.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-k", ToggleCommandPalette, Some("Spool")),
        KeyBinding::new("cmd-shift-p", ToggleCommandPalette, Some("Spool")),
        KeyBinding::new("cmd-f", FocusSearch, Some("Spool")),
        // The everyday one. A full sync is a deliberate act and gets the
        // longer chord.
        KeyBinding::new("cmd-shift-n", GetNewMail, Some("Spool")),
        KeyBinding::new("cmd-alt-shift-n", Sync, Some("Spool")),
        KeyBinding::new("cmd-n", Compose, Some("Spool")),
        KeyBinding::new("cmd-shift-d", ResumeDraft, Some("Spool")),
        KeyBinding::new("cmd-r", Reply, Some("Spool")),
        KeyBinding::new("cmd-shift-r", ReplyAll, Some("Spool")),
        KeyBinding::new("cmd-shift-f", Forward, Some("Spool")),
        // The composer is a window of its own, so its keys are bound in its
        // own context. `Spool` bindings never reach it and its never reach
        // the mailbox.
        KeyBinding::new("cmd-enter", SendMessage, Some("Compose")),
        KeyBinding::new("cmd-shift-backspace", DiscardDraft, Some("Compose")),
        KeyBinding::new("escape", Cancel, Some("Compose")),
        // Settings is a window of its own too, and escape unwinds one layer
        // there: the account form first, then the window.
        KeyBinding::new("escape", Cancel, Some("Settings")),
        // Scoped to the composer: cmd-+ in a window showing a list of five
        // hundred subjects means something else, and this is not it.
        KeyBinding::new("cmd-=", IncreaseComposeFont, Some("Compose")),
        KeyBinding::new("cmd-+", IncreaseComposeFont, Some("Compose")),
        KeyBinding::new("cmd--", DecreaseComposeFont, Some("Compose")),
        KeyBinding::new("cmd-0", ResetComposeFont, Some("Compose")),
        // Bound in the composer's own context, so they resolve only for a
        // field that does not claim them itself. The body is a `TextArea`,
        // whose own context binds all three — enter inserts a newline there
        // and always will.
        KeyBinding::new("down", CompletionNext, Some("Compose")),
        KeyBinding::new("up", CompletionPrev, Some("Compose")),
        KeyBinding::new("enter", AcceptCompletion, Some("Compose")),
        KeyBinding::new("tab", NextField, Some("Compose")),
        KeyBinding::new("shift-tab", PrevField, Some("Compose")),
        // `v` for "move to", the binding Gmail trained everyone on. Junk has
        // no key: marking mail as spam by mistyping one letter is a way to
        // lose it.
        KeyBinding::new("v", MoveToFolder, Some("MessageList")),
        // ⌘⇧S is Save As everywhere else, and ⌘Y is what Finder uses for
        // Quick Look, which is what this opens.
        KeyBinding::new("cmd-shift-s", SaveAttachments, Some("Spool")),
        KeyBinding::new("cmd-y", PreviewAttachment, Some("Spool")),
        KeyBinding::new("cmd-b", ToggleSidebar, Some("Spool")),
        // The other bar. `cmd-b` hides the sidebar, `cmd-alt-b` the status
        // bar — and the status bar is where a sync's progress now lives, so
        // this is the key you reach for to watch one and then put it away
        // again.
        KeyBinding::new("cmd-alt-b", ToggleStatusBar, Some("Spool")),
        KeyBinding::new("cmd-alt-p", ToggleBodySource, Some("Spool")),
        KeyBinding::new("cmd-alt-d", ToggleDensity, Some("Spool")),
        KeyBinding::new("cmd-alt-t", ToggleConversations, Some("Spool")),
        // Bound globally: ⌘, is expected to work whichever window is in
        // front, and only the mailbox knows how to open it.
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("escape", Cancel, Some("Spool")),
        // `enter` reaches this only when the list does not have focus — that
        // is, while the palette is open.
        KeyBinding::new("enter", Confirm, Some("Spool")),
        // Vim-style motion, plus the arrow keys for everyone else.
        KeyBinding::new("j", SelectNext, Some("MessageList")),
        KeyBinding::new("k", SelectPrev, Some("MessageList")),
        KeyBinding::new("down", SelectNext, Some("Spool")),
        KeyBinding::new("up", SelectPrev, Some("Spool")),
        KeyBinding::new("g g", SelectFirst, Some("MessageList")),
        KeyBinding::new("shift-g", SelectLast, Some("MessageList")),
        KeyBinding::new("home", SelectFirst, Some("Spool")),
        KeyBinding::new("end", SelectLast, Some("Spool")),
        KeyBinding::new("pagedown", PageDown, Some("Spool")),
        KeyBinding::new("pageup", PageUp, Some("Spool")),
        KeyBinding::new("ctrl-d", PageDown, Some("MessageList")),
        KeyBinding::new("ctrl-u", PageUp, Some("MessageList")),
        KeyBinding::new("enter", OpenSelected, Some("MessageList")),
        KeyBinding::new("u", ToggleRead, Some("MessageList")),
        // Gmail's pair, and the reason `u` stays the toggle: these say which
        // way, which is what a mixed selection needs.
        KeyBinding::new("shift-i", MarkRead, Some("MessageList")),
        KeyBinding::new("shift-u", MarkUnread, Some("MessageList")),
        KeyBinding::new("f", ToggleFlag, Some("MessageList")),
        KeyBinding::new("e", Archive, Some("MessageList")),
        KeyBinding::new("backspace", DeleteMessage, Some("MessageList")),
        KeyBinding::new("t", ToggleThreadView, Some("MessageList")),
        KeyBinding::new("tab", CycleFilter, Some("MessageList")),
        KeyBinding::new("/", FocusSearch, Some("MessageList")),
        KeyBinding::new("shift-c", CopySender, Some("MessageList")),
        KeyBinding::new("r", Reply, Some("MessageList")),
        KeyBinding::new("a", ReplyAll, Some("MessageList")),
        KeyBinding::new("cmd-a", SelectAllMessages, Some("MessageList")),
        // Scoped to the list: inside the composer, cmd-z belongs to the text
        // being written, not to a message that was filed a minute ago.
        KeyBinding::new("cmd-z", Undo, Some("MessageList")),
        KeyBinding::new("shift-down", ExtendNext, Some("MessageList")),
        KeyBinding::new("shift-up", ExtendPrev, Some("MessageList")),
        KeyBinding::new("shift-j", ExtendNext, Some("MessageList")),
        KeyBinding::new("shift-k", ExtendPrev, Some("MessageList")),
        KeyBinding::new("cmd-shift-a", MarkAllRead, Some("Spool")),
        KeyBinding::new("cmd-shift-m", NewMailbox, Some("Spool")),
        KeyBinding::new("ctrl-n", NextMailbox, Some("Spool")),
        // Finder's list view, in the sidebar: up and down walk the rows,
        // right unfolds or steps in, left folds or steps out. Left from the
        // list and return from the sidebar pass the keyboard between the two.
        KeyBinding::new("left", FocusSidebar, Some("MessageList")),
        KeyBinding::new("down", SidebarNext, Some("Sidebar")),
        KeyBinding::new("up", SidebarPrev, Some("Sidebar")),
        KeyBinding::new("j", SidebarNext, Some("Sidebar")),
        KeyBinding::new("k", SidebarPrev, Some("Sidebar")),
        KeyBinding::new("right", SidebarExpand, Some("Sidebar")),
        KeyBinding::new("left", SidebarCollapse, Some("Sidebar")),
        KeyBinding::new("enter", FocusMessageList, Some("Sidebar")),
        KeyBinding::new("escape", FocusMessageList, Some("Sidebar")),
        KeyBinding::new("ctrl-p", PrevMailbox, Some("Spool")),
        // Clients is a window of its own, opened from any window — the
        // mailbox passes it the sender of the message being read.
        KeyBinding::new("cmd-shift-c", OpenClients, None),
        KeyBinding::new("cmd-1", ShowClientDirectory, Some("Clients")),
        KeyBinding::new("cmd-2", ShowClientBoard, Some("Clients")),
        KeyBinding::new("cmd-f", FocusSearch, Some("Clients")),
        KeyBinding::new("escape", Cancel, Some("Clients")),
        KeyBinding::new("down", SelectNext, Some("Clients")),
        KeyBinding::new("up", SelectPrev, Some("Clients")),
        KeyBinding::new("j", SelectNext, Some("ClientList")),
        KeyBinding::new("k", SelectPrev, Some("ClientList")),
        // Picking a run of people, to move them all to a stage at once.
        KeyBinding::new("shift-down", ExtendNext, Some("ClientList")),
        KeyBinding::new("shift-up", ExtendPrev, Some("ClientList")),
        KeyBinding::new("cmd-n", NewClient, Some("Clients")),
        KeyBinding::new("cmd-shift-i", ImportContacts, Some("Clients")),
        // Brackets, because they point: the card goes the way they do.
        KeyBinding::new("[", StageLeft, Some("ClientList")),
        KeyBinding::new("]", StageRight, Some("ClientList")),
        KeyBinding::new("[", StageLeft, Some("ClientBoard")),
        KeyBinding::new("]", StageRight, Some("ClientBoard")),
        // Scoped like the mailbox's: in a text field, ⌘Z is the field's.
        KeyBinding::new("cmd-z", Undo, Some("ClientList")),
        KeyBinding::new("cmd-z", Undo, Some("ClientBoard")),
        KeyBinding::new("cmd-enter", SaveNote, Some("ClientNote")),
        // Return in a one-line field of Clients adds what was typed.
        KeyBinding::new("enter", Confirm, Some("ClientField")),
    ]);
}

/// One row in the command palette: a label, the action it dispatches, and the
/// keystroke to advertise.
pub struct Command {
    /// Owned rather than `&'static str`: the palette also lists folders, and
    /// a folder's name comes from the server.
    pub name: gpui::SharedString,
    /// What to show on the right of the row. Owned for the same reason the
    /// name is: an extension's keystroke comes out of a file.
    pub keystroke: gpui::SharedString,
    pub action: Box<dyn gpui::Action>,
}

/// Runs a command an extension contributed.
///
/// The one action in this file that carries data, because it has to: an
/// extension's commands are not known until its manifest is read, so there is
/// no unit struct to declare for them. The id is the qualified one the registry
/// produces — `spam-scorer.file` — which is what makes two extensions both
/// contributing a `file` dispatchable to the right one.
///
/// `no_json` because these are never written in a keymap file by hand. The
/// keystroke an extension asks for is bound from its manifest, and everything
/// else reaches this through the palette.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = mailbox, no_json)]
pub struct RunExtensionCommand {
    pub id: String,
}

/// Keystrokes this app binds for itself.
///
/// An extension may ask for a keystroke in its manifest, and gets it only if
/// it is not one of these — an extension that could take `cmd-z` away from
/// undo by being installed is an extension that breaks the app by existing.
///
/// A list beside [`bind_keys`] rather than derived from it, because a
/// `KeyBinding` does not hand back the keystroke it was built from. The test
/// below reads this file and fails if the two ever disagree, so the duplicate
/// cannot rot.
pub fn claimed_keystrokes() -> std::collections::BTreeSet<&'static str> {
    CLAIMED.iter().copied().collect()
}

const CLAIMED: &[&str] = &[
    "/", "[", "]", "a", "backspace", "cmd-1", "cmd-2", "cmd-shift-c", "cmd-shift-i", "cmd-+", "cmd-,", "cmd--", "cmd-0", "cmd-=", "cmd-a", "cmd-alt-b",
    "cmd-alt-d", "cmd-alt-p", "cmd-alt-shift-n", "cmd-alt-t", "cmd-b", "cmd-enter", "cmd-f",
    "cmd-k", "cmd-n", "cmd-q", "cmd-r", "cmd-shift-a", "cmd-shift-backspace", "cmd-shift-d",
    "cmd-shift-f", "cmd-shift-m", "cmd-shift-n", "cmd-shift-p", "cmd-shift-r", "cmd-shift-s",
    "cmd-y", "cmd-z", "ctrl-d", "ctrl-n", "ctrl-p", "ctrl-q", "ctrl-u", "down", "e", "end",
    "enter", "escape", "f", "g g", "home", "j", "k", "left", "pagedown", "pageup", "r", "right", "shift-c",
    "shift-down", "shift-g", "shift-i", "shift-j", "shift-k", "shift-tab", "shift-u",
    "shift-up", "t", "tab", "u", "up", "v",
];

/// The palette's contents. Ordered by how often you would reach for them.
pub fn commands() -> Vec<Command> {
    fn cmd(name: &'static str, keystroke: &'static str, action: impl gpui::Action) -> Command {
        Command { name: name.into(), keystroke: keystroke.into(), action: Box::new(action) }
    }

    vec![
        cmd("Search messages", "cmd-f", FocusSearch),
        cmd("New message", "cmd-n", Compose),
        cmd("Resume the saved draft", "cmd-shift-d", ResumeDraft),
        cmd("Reply", "cmd-r", Reply),
        cmd("Reply all", "cmd-shift-r", ReplyAll),
        cmd("Forward", "cmd-shift-f", Forward),
        cmd("Get new mail \u{2014} inboxes only", "cmd-shift-n", GetNewMail),
        cmd("Sync everything \u{2014} every folder", "cmd-alt-shift-n", Sync),
        cmd("Load older messages", "", LoadOlder),
        cmd("Filter: all / unread / flagged", "tab", CycleFilter),
        cmd("Toggle read", "u", ToggleRead),
        cmd("Mark as read", "shift-i", MarkRead),
        cmd("Mark as unread", "shift-u", MarkUnread),
        cmd("Toggle flag", "f", ToggleFlag),
        cmd("Archive message", "e", Archive),
        cmd("Move to folder\u{2026}", "v", MoveToFolder),
        cmd("Mark as spam", "", MarkAsSpam),
        cmd("Not spam", "", NotSpam),
        cmd("Unsubscribe from this list", "", Unsubscribe),
        cmd("Delete message", "backspace", DeleteMessage),
        cmd("Save attachments to Downloads", "cmd-shift-s", SaveAttachments),
        cmd("Quick Look the first attachment", "cmd-y", PreviewAttachment),
        cmd("Undo the last move", "cmd-z", Undo),
        cmd("New folder\u{2026}", "cmd-shift-m", NewMailbox),
        cmd("Rename this folder\u{2026}", "", RenameMailbox),
        cmd("Delete this folder\u{2026}", "", DeleteMailbox),
        cmd("Mark mailbox read", "cmd-shift-a", MarkAllRead),
        cmd("Show whole conversation", "t", ToggleThreadView),
        cmd("Toggle formatted / plain text", "cmd-alt-p", ToggleBodySource),
        cmd("Message list: slim / detail", "cmd-alt-d", ToggleDensity),
        cmd("Group into conversations", "cmd-alt-t", ToggleConversations),
        cmd("Next mailbox", "ctrl-n", NextMailbox),
        cmd("Previous mailbox", "ctrl-p", PrevMailbox),
        cmd("Select all messages", "cmd-a", SelectAllMessages),
        cmd("Copy sender address", "shift-c", CopySender),
        cmd("Open sender in Clients", "cmd-shift-c", OpenClients),
        cmd("Clients: the board", "", OpenClientBoard),
        cmd("Toggle sidebar", "cmd-b", ToggleSidebar),
        cmd("Toggle status bar", "cmd-alt-b", ToggleStatusBar),
        cmd("Show / hide All Mail", "", ToggleUnifiedInbox),
        cmd("Settings: appearance and theme", "cmd-,", OpenSettings),
        cmd("Go to newest", "g g", SelectFirst),
        cmd("Go to oldest", "shift-g", SelectLast),
        cmd("Check for updates\u{2026}", "", CheckForUpdates),
        cmd("Make Spool the default email app", "", MakeDefaultMailApp),
        cmd("Quit", "cmd-q", Quit),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The duplicate that cannot rot.
    ///
    /// `CLAIMED` exists because a `KeyBinding` will not say what keystroke it
    /// was built from, and an extension asking for `cmd-z` has to be refused
    /// against *something*. This reads the source of `bind_keys` and fails if
    /// the two lists ever disagree — in either direction, since a keystroke
    /// listed here and bound nowhere would refuse an extension for no reason.
    #[test]
    fn every_keystroke_this_app_binds_is_one_an_extension_cannot_take() {
        let source = include_str!("actions.rs");
        let body = source
            .split_once("pub fn bind_keys")
            .expect("bind_keys is in this file")
            .1;

        let mut bound = std::collections::BTreeSet::new();
        for piece in body.split("KeyBinding::new(\"").skip(1) {
            if let Some((keystroke, _)) = piece.split_once('"') {
                bound.insert(keystroke);
            }
        }

        assert!(!bound.is_empty(), "the parse found nothing, so it is not checking anything");
        assert_eq!(
            bound,
            claimed_keystrokes(),
            "CLAIMED and bind_keys disagree; an extension would be given a key the app uses, \
             or refused one it does not",
        );
    }
}
