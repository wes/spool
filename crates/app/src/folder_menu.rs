//! The menu you get by right-clicking a folder.
//!
//! Creating, renaming and deleting mailboxes already existed — over IMAP, with
//! the local store following the server — but the only ways in were a `+` on
//! the account row and two File-menu items acting on "the selected folder".
//! That is management you have to already know about: nothing on a folder says
//! it can be renamed, and a menu at the top of the screen does not say which
//! folder it means.
//!
//! So: right-click the row itself. It acts on the row under the pointer rather
//! than on the selection, which is why every item carries its own target —
//! right-clicking a folder to rename it should not first move you into it and
//! load a thousand messages you did not ask for.
//!
//! gpui has no menu primitive (Zed's lives in its own `ui` crate), so this is
//! a small one: an anchored popover, a scrim that swallows the next click, and
//! rows that dispatch straight into `folder_ui`.

use gpui::{
    AnyElement, Context, MouseButton, Pixels, Point, SharedString, Window, anchored, div,
    prelude::*, px,
};
use spool_core::{AccountId, MailboxId, MailboxRole};

use crate::{folder_ui::FolderAction, theme::Theme, workspace::Workspace};

/// What was right-clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTarget {
    /// An account header: the only thing to do here is add a folder to it.
    Account(AccountId),
    Folder(MailboxId),
}

/// An open context menu.
pub struct FolderMenu {
    pub target: MenuTarget,
    /// Where the pointer was, in window coordinates.
    pub at: Point<Pixels>,
}

impl Workspace {
    /// Opens the menu for a sidebar row.
    pub fn open_folder_menu(
        &mut self,
        target: MenuTarget,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.folder_menu = Some(FolderMenu { target, at });
        cx.notify();
    }

    pub fn close_folder_menu(&mut self, cx: &mut Context<Self>) {
        if self.folder_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn render_folder_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.folder_menu.as_ref()?;

        // The account the new folder would belong to, and the folder the other
        // two would act on.
        let (account, folder) = match menu.target {
            MenuTarget::Account(account) => (Some(account), None),
            MenuTarget::Folder(id) => {
                let folder = self.mailboxes.iter().find(|m| m.id == id)?;
                (Some(folder.account_id), Some(folder))
            }
        };

        // Special-use folders are load-bearing — sync files mail into them by
        // role — and the server would refuse anyway. Shown greyed rather than
        // hidden: an item that is missing is a mystery, one that is dimmed
        // with a reason beside it is an answer.
        let protected = folder.is_some_and(|f| is_protected(f.role));
        let name: SharedString = folder.map(|f| f.name.clone()).unwrap_or_default().into();

        let mut items: Vec<AnyElement> = Vec::new();

        if let Some(account) = account {
            items.push(
                self.folder_menu_item(theme, "New Folder\u{2026}", true, cx, move |this, window, cx| {
                    this.open_folder_prompt(FolderAction::Create { account }, window, cx);
                })
                .into_any_element(),
            );
        }

        if let Some(folder) = folder {
            let id = folder.id;
            items.push(divider(theme).into_any_element());
            items.push(
                self.folder_menu_item(theme, "Rename\u{2026}", !protected, cx, move |this, window, cx| {
                    this.open_folder_prompt(FolderAction::Rename { mailbox: id }, window, cx);
                })
                .into_any_element(),
            );
            items.push(
                self.folder_menu_item(theme, "Delete\u{2026}", !protected, cx, move |this, window, cx| {
                    this.open_folder_prompt(FolderAction::Delete { mailbox: id }, window, cx);
                })
                .into_any_element(),
            );

            if protected {
                items.push(
                    div()
                        .px_3()
                        .pt_1()
                        .pb_0p5()
                        .max_w(px(200.))
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "{name} is a special folder and cannot be changed"
                        )))
                        .into_any_element(),
                );
            }
        }

        Some(
            // The same shape as the palette's scrim, and for the same reason:
            // a plain absolute overlay is what actually paints over the panes
            // drawn before it. Wrapping this in `deferred` collapsed it to
            // nothing — the layout survives, the paint does not.
            div()
                // A scrim over the whole window: the next click anywhere
                // dismisses, which is what every context menu does and what
                // stops one being left open behind a dialog.
                .id("folder-menu-scrim")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_folder_menu(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.close_folder_menu(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    // `anchored` keeps it inside the window, so a right-click
                    // near the bottom edge opens upwards instead of off the
                    // screen.
                    anchored().position(menu.at).child(
                        div()
                            .flex()
                            .flex_col()
                            .py_1()
                            .min_w(px(160.))
                            .rounded_md()
                            .bg(theme.elevated)
                            .border_1()
                            .border_color(theme.border)
                            .shadow_lg()
                            .text_size(theme.text_size)
                            // Swallowed here, or the scrim underneath would
                            // dismiss the menu before the item's own click ran.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            })
                            .children(items),
                    ),
                )
                .into_any_element(),
        )
    }

    fn folder_menu_item(
        &self,
        theme: &Theme,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(label)
            .px_3()
            .py_1()
            .when(enabled, |this| {
                this.cursor_pointer()
                    .text_color(theme.text)
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Closed first: the prompt that follows takes focus,
                        // and a menu still open behind it is a menu nothing
                        // will dismiss.
                        this.close_folder_menu(cx);
                        run(this, window, cx);
                    }))
            })
            .when(!enabled, |this| this.text_color(theme.text_faint))
            .child(label)
    }
}

fn divider(theme: &Theme) -> gpui::Div {
    div().my_1().h(px(1.)).bg(theme.border_subtle)
}

/// Whether a folder is one the app and the server both depend on.
///
/// Sync files mail into these by role, and the server would refuse the rename
/// or delete anyway. The same rule `folder_ui` applies when an action arrives
/// from the menu bar — kept here as a function so the menu greys the items out
/// for exactly the folders the action would reject, rather than offering
/// something that fails a moment later.
fn is_protected(role: MailboxRole) -> bool {
    role != MailboxRole::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_you_made_can_be_renamed_and_deleted() {
        assert!(!is_protected(MailboxRole::Other));
    }

    #[test]
    fn the_folders_the_app_files_mail_into_cannot_be_touched() {
        // Losing any of these is not a folder gone: it is sync with nowhere to
        // put archived mail, or a delete with no Trash to move to.
        for role in [
            MailboxRole::Inbox,
            MailboxRole::Sent,
            MailboxRole::Drafts,
            MailboxRole::Archive,
            MailboxRole::Trash,
            MailboxRole::Junk,
        ] {
            assert!(is_protected(role), "{role:?} must not be renameable");
        }
    }
}
