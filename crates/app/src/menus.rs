//! The system menu bar.
//!
//! Almost nothing here is new behaviour. Every operation is already a gpui
//! [`Action`](gpui::Action), so a menu item is a label pointing at one — which
//! is the same thing the keymap and the command palette do. Adding an
//! operation in `actions.rs` makes it reachable four ways, and this file is
//! where the fourth is spelled out.
//!
//! The exceptions are the window-management items. macOS expects ⌘W, ⌘M and
//! Zoom to exist in every app, and they have no equivalent anywhere else in
//! Spool, so they are defined here rather than in `actions.rs` — nothing but
//! a menu would ever dispatch them.

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType, actions};

use crate::{actions::*, text_input};

actions!(
    window,
    [
        /// Closes the focused window.
        CloseWindow,
        Minimize,
        Zoom,
        ToggleFullScreen,
        /// Hides Spool. macOS binds this to ⌘H whether or not an app asks.
        Hide,
        HideOthers,
        ShowAll,
    ]
);

/// Registers the handlers for the window items, then installs the menu bar.
///
/// Called once at startup, after the keymap is bound. `clients` is whether
/// Clients has been added, which is whether the Window menu offers it.
pub fn init(clients: bool, cx: &mut App) {
    cx.on_action(|_: &CloseWindow, cx: &mut App| {
        // The fallback, for a window with nothing to do on the way out. The
        // mailbox, a composer and the settings sheet each handle `CloseWindow`
        // themselves, and an element's handler wins over this one.
        //
        // Closing the last window quits — that is `on_window_closed` in
        // `main`, which catches the red button too, not something to repeat
        // here.
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.remove_window()).ok();
        }
    });

    cx.on_action(|_: &Minimize, cx: &mut App| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.minimize_window()).ok();
        }
    });

    cx.on_action(|_: &Zoom, cx: &mut App| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.zoom_window()).ok();
        }
    });

    cx.on_action(|_: &ToggleFullScreen, cx: &mut App| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.toggle_fullscreen()).ok();
        }
    });

    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());

    refresh(clients, cx);
}

/// Installs the menu bar again, for when Clients has been added or removed.
pub fn refresh(clients: bool, cx: &mut App) {
    cx.set_menus(menus(clients));
}

/// The menu bar, top level left to right.
fn menus(clients: bool) -> Vec<Menu> {
    vec![app_menu(), file_menu(), edit_menu(), view_menu(), message_menu(), window_menu(clients)]
}

fn app_menu() -> Menu {
    Menu {
        name: "Spool".into(),
        items: vec![
            // First, as in every Mac app.
            MenuItem::action("About Spool", AboutSpool),
            MenuItem::separator(),
            // Above Settings, which is where macOS apps put it: it is about
            // the app itself rather than about how it behaves.
            MenuItem::action("Check for Updates\u{2026}", CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Settings\u{2026}", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Spool", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Spool", Quit),
        ],
    }
}

fn file_menu() -> Menu {
    Menu {
        name: "File".into(),
        items: vec![
            MenuItem::action("New Message", Compose),
            MenuItem::action("Resume Draft", ResumeDraft),
            MenuItem::separator(),
            // Folder management sits in File rather than in a menu of its own:
            // it is rare, and a top-level "Spool" menu next to the app menu
            // of the same name would be one word meaning two things.
            MenuItem::action("New Folder\u{2026}", NewMailbox),
            MenuItem::action("Rename Folder\u{2026}", RenameMailbox),
            MenuItem::action("Delete Folder\u{2026}", DeleteMailbox),
            MenuItem::separator(),
            MenuItem::action("Reply", Reply),
            MenuItem::action("Reply All", ReplyAll),
            MenuItem::action("Forward", Forward),
            MenuItem::separator(),
            MenuItem::action("Send", SendMessage),
            MenuItem::separator(),
            MenuItem::action("Get New Mail", GetNewMail),
            MenuItem::action("Sync Now", Sync),
            MenuItem::action("Load Older Messages", LoadOlder),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ],
    }
}

/// The Edit menu.
///
/// The `os_action` on each item is what makes macOS treat these as the real
/// system editing commands: it wires them into the responder chain, so a
/// native text field, the character palette and dictation all behave. Without
/// it the items would appear but the system would not know what they mean.
fn edit_menu() -> Menu {
    Menu {
        name: "Edit".into(),
        items: vec![
            // Undo files a message back where it came from. It carries the
            // OS action so macOS treats it as the real Undo item, and the
            // keybinding is scoped to the message list so a composer keeps
            // cmd-z for the text being written.
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", text_input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", text_input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", text_input::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", text_input::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find\u{2026}", FocusSearch),
            MenuItem::separator(),
            MenuItem::action("Emoji & Symbols", text_input::ShowCharacterPalette),
        ],
    }
}

fn view_menu() -> Menu {
    Menu {
        name: "View".into(),
        items: vec![
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Status Bar", ToggleStatusBar),
            MenuItem::action("Show All Mail", ToggleUnifiedInbox),
            MenuItem::action("Message List Density", ToggleDensity),
            MenuItem::action("Group by Conversation", ToggleConversations),
            MenuItem::separator(),
            // Only a composer answers these; the mailbox window ignores them.
            MenuItem::action("Bigger Compose Text", IncreaseComposeFont),
            MenuItem::action("Smaller Compose Text", DecreaseComposeFont),
            MenuItem::action("Default Compose Text", ResetComposeFont),
            MenuItem::separator(),
            MenuItem::action("Show Conversation", ToggleThreadView),
            MenuItem::action("Formatted / Plain Text", ToggleBodySource),
            MenuItem::separator(),
            MenuItem::action("Filter: All / Unread / Flagged", CycleFilter),
            MenuItem::separator(),
            MenuItem::action("Command Palette\u{2026}", ToggleCommandPalette),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullScreen),
        ],
    }
}

fn message_menu() -> Menu {
    Menu {
        name: "Message".into(),
        items: vec![
            MenuItem::action("Open", OpenSelected),
            MenuItem::separator(),
            MenuItem::action("Quick Look Attachment", PreviewAttachment),
            MenuItem::action("Save Attachments to Downloads", SaveAttachments),
            MenuItem::separator(),
            MenuItem::action("Mark as Read", MarkRead),
            MenuItem::action("Mark as Unread", MarkUnread),
            MenuItem::action("Toggle Read / Unread", ToggleRead),
            MenuItem::action("Flag", ToggleFlag),
            MenuItem::action("Mark All as Read", MarkAllRead),
            MenuItem::separator(),
            MenuItem::action("Archive", Archive),
            MenuItem::action("Move to Folder\u{2026}", MoveToFolder),
            MenuItem::action("Move to Trash", DeleteMessage),
            MenuItem::separator(),
            MenuItem::action("Mark as Spam", MarkAsSpam),
            MenuItem::action("Not Spam", NotSpam),
            MenuItem::separator(),
            MenuItem::action("Copy Sender Address", CopySender),
        ],
    }
}

fn window_menu(clients: bool) -> Menu {
    let mut items = vec![
        MenuItem::action("Minimize", Minimize),
        MenuItem::action("Zoom", Zoom),
        MenuItem::separator(),
        MenuItem::action("Next Mailbox", NextMailbox),
        MenuItem::action("Previous Mailbox", PrevMailbox),
    ];
    if clients {
        items.extend([
            MenuItem::separator(),
            MenuItem::action("Clients", OpenClients),
            MenuItem::action("Clients Board", OpenClientBoard),
        ]);
    }
    Menu { name: "Window".into(), items }
}

/// The keystrokes macOS expects on menu items.
///
/// Bound separately from the menu because gpui reads a menu item's shortcut
/// out of the keymap: an item shows a shortcut only when its action has one,
/// so these have to exist for ⌘W and ⌘M to appear next to their items rather
/// than the items looking unbound.
pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding as K;

    cx.bind_keys([
        K::new("cmd-w", CloseWindow, None),
        K::new("cmd-m", Minimize, None),
        K::new("cmd-h", Hide, None),
        K::new("cmd-alt-h", HideOthers, None),
        K::new("ctrl-cmd-f", ToggleFullScreen, None),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks every item, including submenus, so a nested item is not missed.
    fn walk(items: &[MenuItem], visit: &mut impl FnMut(&MenuItem)) {
        for item in items {
            visit(item);
            if let MenuItem::Submenu(menu) = item {
                walk(&menu.items, visit);
            }
        }
    }

    fn item_names(menus: &[Menu]) -> Vec<String> {
        let mut names = Vec::new();
        for menu in menus {
            walk(&menu.items, &mut |item| {
                if let MenuItem::Action { name, .. } = item {
                    names.push(name.to_string());
                }
            });
        }
        names
    }

    #[test]
    fn clients_is_in_the_window_menu_only_once_it_has_been_added() {
        let without = item_names(&menus(false));
        let with = item_names(&menus(true));
        assert!(!without.iter().any(|n| n.starts_with("Clients")), "{without:?}");
        assert!(with.iter().any(|n| n == "Clients") && with.iter().any(|n| n == "Clients Board"), "{with:?}");
    }

    #[test]
    fn the_expected_top_level_menus_are_present() {
        let names: Vec<String> = menus(false).iter().map(|m| m.name.to_string()).collect();
        assert_eq!(names, ["Spool", "File", "Edit", "View", "Message", "Window"]);
    }

    #[test]
    fn the_items_macos_users_reach_for_exist() {
        let names = item_names(&menus(false));
        for expected in [
            "Quit Spool",
            "Hide Spool",
            "Close Window",
            "Minimize",
            "Zoom",
            "Undo",
            "Toggle Status Bar",
            "Cut",
            "Copy",
            "Paste",
            "Select All",
            "Archive",
            "Move to Trash",
            "New Message",
            "New Folder\u{2026}",
            "Reply",
            "Reply All",
            "Send",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected:?} from {names:?}");
        }
    }

    #[test]
    fn the_editing_items_carry_their_os_actions() {
        // Without these macOS does not treat the items as the system editing
        // commands, and native text handling stops working.
        let mut with_os_action = Vec::new();
        for menu in menus(false) {
            walk(&menu.items, &mut |item| {
                if let MenuItem::Action { name, os_action: Some(_), .. } = item {
                    with_os_action.push(name.to_string());
                }
            });
        }

        for expected in ["Undo", "Cut", "Copy", "Paste", "Select All"] {
            assert!(
                with_os_action.iter().any(|name| name == expected),
                "{expected:?} should carry an OsAction, got {with_os_action:?}",
            );
        }
    }

    #[test]
    fn no_menu_is_empty_and_none_ends_on_a_separator() {
        for menu in menus(false).into_iter().chain(menus(true)) {
            assert!(!menu.items.is_empty(), "{} is empty", menu.name);
            assert!(
                !matches!(menu.items.last(), Some(MenuItem::Separator)),
                "{} ends on a separator, which draws as a stray line",
                menu.name,
            );
            assert!(
                !matches!(menu.items.first(), Some(MenuItem::Separator)),
                "{} starts with a separator",
                menu.name,
            );
        }
    }

    #[test]
    fn separators_never_double_up() {
        for menu in menus(false).into_iter().chain(menus(true)) {
            for pair in menu.items.windows(2) {
                assert!(
                    !matches!((&pair[0], &pair[1]), (MenuItem::Separator, MenuItem::Separator)),
                    "{} has two separators in a row",
                    menu.name,
                );
            }
        }
    }
}
