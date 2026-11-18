//! The account and folder sidebar.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Hsla, SharedString, Window, div, ease_in_out,
    prelude::*, px,
};
use spool_core::{Account, Mailbox, MailboxRole};

use crate::folder_menu::MenuTarget;
use crate::sidebar_nav::RowKey;
use crate::update::Stage;
use spool_store::Scope;

use crate::{
    perf::format_count,
    theme::Theme,
    workspace::{DraggedMessages, Workspace},
};

/// The tallest an extension's panel may be in the sidebar.
///
/// A panel that could take the whole sidebar is a panel that can hide the
/// folders, and the folders are what the sidebar is for.
const PANEL_MAX_HEIGHT: f32 = 240.;

impl Workspace {
    pub(crate) fn render_sidebar(
        &self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The row the keyboard is on, while the sidebar has the keyboard.
        let ring = self.sidebar_ring(window);

        // Group folders under their account, preserving the order the store
        // returned them in (role first, then name). Built eagerly: a lazy
        // iterator would have to keep `cx` borrowed until it is consumed.
        let mut sections = Vec::with_capacity(self.accounts.len());
        // The order the accounts have been dragged into, which is the one
        // piece of the sidebar's order that is the reader's rather than the
        // server's.
        let accounts = self.sorted_accounts();
        let emails: std::rc::Rc<Vec<String>> =
            std::rc::Rc::new(accounts.iter().map(|a| a.email.clone()).collect());
        for account in accounts {
            let folders: Vec<&Mailbox> = self
                .mailboxes
                .iter()
                .filter(|m| m.account_id == account.id)
                .collect();
            sections.push(self.render_account_section(
                account,
                &folders,
                &emails,
                ring.as_ref(),
                theme,
                cx,
            ));
        }

        div()
            .flex()
            .flex_col()
            .w(px(self.sidebar_width()))
            .flex_shrink_0()
            .h_full()
            .bg(theme.panel)
            // The sidebar runs the whole height of the window, so its own top
            // strip is where the traffic lights live. No rule crosses it: that
            // continuous panel down the side of the window is most of what
            // makes it read as a Mac app rather than a web page.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(crate::chrome::HEADER_HEIGHT))
                    .flex_shrink_0()
                    .pl(px(crate::chrome::TRAFFIC_LIGHT_INSET))
                    .pr_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.small_text_size)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child("Spool"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    // Takes the keyboard when clicked, as Finder's sidebar
                    // does, so the arrows move through what was clicked in.
                    .key_context("Sidebar")
                    .track_focus(&self.sidebar_focus)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, _| window.focus(&this.sidebar_focus)),
                    )
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_2()
                    .children(
                        self.settings
                            .unified_inbox
                            .then(|| self.render_unified_row(ring.as_ref(), theme, cx)),
                    )
                    .children(sections)
                    // Below the folders, because a folder is where mail is and
                    // an extension's panel is something about it.
                    .children(self.render_extension_panels(theme)),
            )
            .children(self.render_demo_card(theme, cx))
            .children(self.render_default_mail_offer(theme, cx))
            .children(self.settings.clients.then(|| self.render_clients_row(theme, cx)))
            .children(self.render_update(theme, cx))
    }

    /// Clients, once it has been added: a link to its window, pinned under
    /// the folders rather than after them, because with a few accounts open
    /// the end of the folders is a long scroll away. Laid out as a folder row
    /// so it lines up with them.
    ///
    /// Never selected, and not on the keyboard's path through the sidebar:
    /// the arrows select what they land on, and landing here would open a
    /// window.
    fn render_clients_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_shrink_0()
            .px_2()
            .pt_1()
            .child(
                div()
                    .id("sidebar-clients")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    // The same transparent border as a folder row, so the two
                    // line up to the pixel.
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(|this, _, _, cx| this.open_clients(cx)))
                    .child(div().w(px(TWISTY_WIDTH)).flex_shrink_0())
                    .child(
                        div().w(px(14.)).flex_shrink_0().child(
                            gpui::svg()
                                .path(crate::icons::PERSON)
                                .size(px(13.))
                                .text_color(theme.text_faint),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text_muted)
                            .child("Clients"),
                    ),
            )
            .into_any_element()
    }

    /// What extensions contribute to the sidebar.
    ///
    /// One section per contributed panel, drawn from the last tree that panel
    /// described. The section header comes from the manifest, so it is there
    /// from the first frame — the panel below it fills in once the extension
    /// has been asked, which is a frame or two later.
    ///
    /// Each is capped in height and scrolls. A panel is an extension's corner
    /// of the sidebar, not a claim on all of it.
    fn render_extension_panels(&self, theme: &Theme) -> Vec<AnyElement> {
        let Some(host) = &self.extensions else {
            return Vec::new();
        };

        host.panels()
            .into_iter()
            .filter_map(|contributed| {
                let tree = host.panel(&contributed.qualified)?;
                if tree.is_empty() {
                    return None;
                }
                // The panel's buttons run that extension's commands, and a
                // command is qualified by the extension that owns it.
                let (extension, _) = spool_ext::manifest::split_qualified(&contributed.qualified)?;

                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_3()
                        .pt_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_size(theme.small_text_size)
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text_faint)
                                .children(contributed.glyph.clone().map(|g| div().child(g)))
                                .child(contributed.title.clone()),
                        )
                        .child(
                            div()
                                .id(gpui::SharedString::from(format!(
                                    "panel-{}",
                                    contributed.qualified
                                )))
                                .max_h(px(PANEL_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .child(crate::extension_ui::panel(&tree, extension, theme)),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// A new version, at the foot of the sidebar.
    ///
    /// Beside the activity strip rather than in a dialog, and for the same
    /// reason that strip exists: an update is news, not an interruption. A
    /// modal in front of the mail would make the one thing nobody asked for
    /// the one thing they have to deal with first.
    ///
    /// Nothing at all when there is nothing to say. A background check that
    /// found the current version already installed has no business taking a
    /// band of the sidebar to report it.
    /// The foot of the sidebar: which Spool this is, and — when there is a
    /// newer one — the button that installs it.
    ///
    /// Always there, so the version is somewhere to be read without an About
    /// window; quiet, so a copy that is up to date says only its number.
    fn render_update(&self, theme: &Theme, cx: &mut Context<Workspace>) -> Option<AnyElement> {
        let state = self.update.as_ref();
        let failed = state.is_some_and(|s| matches!(s.stage, Stage::Failed(_)));
        let action = state.and_then(|s| s.action());
        let status = state.and_then(|s| s.status());
        // What went wrong, on hover, rather than a sentence squeezed into the
        // width of the sidebar.
        let why: Option<SharedString> = state.filter(|_| failed).map(|s| s.label().into());

        Some(
            div()
                .id("update")
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .text_size(theme.small_text_size)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("{}", crate::update::current()))),
                )
                .children(status.map(|status| {
                    div()
                        .id("update-status")
                        .flex_shrink_0()
                        .text_color(if failed { theme.red } else { theme.text_faint })
                        .when_some(why.clone(), |s, why| {
                            s.tooltip(move |_, cx| {
                                cx.new(|_| crate::chrome::Tip { text: why.clone() }).into()
                            })
                        })
                        .child(status)
                }))
                .children(action.map(|label| {
                    div()
                        .id("update-action")
                        .flex_shrink_0()
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .hover(|s| s.underline())
                        .on_click(cx.listener(|workspace, _, _, cx| workspace.advance_update(cx)))
                        .child(label)
                }))
                .into_any_element(),
        )
    }

    /// The unified inbox: everything, everywhere. Sits above the accounts
    /// because it is the view you want when you are triaging rather than
    /// working in one context.
    ///
    /// Off by default — see `Settings::unified_inbox`.
    fn render_unified_row(
        &self,
        ring: Option<&RowKey>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let unread: u32 = self.mailboxes.iter().map(|m| m.unread).sum();
        let total: u32 = self.mailboxes.iter().map(|m| m.total).sum();

        div()
            .px_2()
            .pb_1()
            .child(
                folder_row(
                    "all-mail",
                    "\u{2756}",
                    "All Mail".into(),
                    unread,
                    total,
                    self.scope == Scope::Everything,
                    // Every folder at once: a row that spun through every sync would
                    // be a permanent fixture rather than a signal.
                    Corner::Count,
                    theme.text_muted,
                    theme,
                    0,
                    None,
                    cx.listener(|this, _, _, cx| {
                        this.sidebar_cursor = Some(RowKey::AllMail);
                        this.select_scope(Scope::Everything, cx)
                    }),
                )
                .when(ring == Some(&RowKey::AllMail), |row| row.bg(theme.selected))
                .when(ring.is_none() && self.scope == Scope::Everything, |row| {
                    row.bg(theme.selected_inactive)
                }),
            )
            .into_any_element()
    }

    fn render_account_section(
        &self,
        account: &Account,
        folders: &[&Mailbox],
        emails: &std::rc::Rc<Vec<String>>,
        ring: Option<&RowKey>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = theme.account_color(account.color);
        let account_id = account.id;
        let email = account.email.clone();
        let collapsed = self.settings.sidebar.account_collapsed(&email);

        // What a collapsed account is hiding. Shown only when it is hiding
        // something: the point of folding an account away is that it stops
        // asking for attention, and a row of zeroes is still asking.
        //
        // The inbox's unread, not the account's. Summing every folder counts
        // Junk, and Archive, and on Gmail the copy of each message that lives
        // in All Mail as well — a number far larger than the amount of mail
        // waiting, attached to the row you fold precisely because you did not
        // want to think about that account right now.
        let hidden_unread: u32 = match collapsed {
            true => folders
                .iter()
                .filter(|m| m.role == MailboxRole::Inbox)
                .map(|m| m.unread)
                .sum(),
            false => 0,
        };

        // What is wrong with the account's connection, if anything, in the
        // words its mark shows on hover.
        let problem: Option<SharedString> = self
            .account_problems
            .get(&crate::workspace::account_key(&email))
            .map(|problem| problem.summary().into());
        let has_inbox = folders.iter().any(|m| m.role == MailboxRole::Inbox);

        let twisty_email = email.clone();
        let dragged = crate::workspace::DraggedAccount {
            email: email.clone().into(),
            label: account.name.clone().into(),
        };
        let onto = email.clone();
        let line = theme.accent;
        let order = emails.clone();

        let header = div()
            .id(SharedString::from(format!("account-{}", account.id)))
            // Accounts are dragged into the order you want them read in. It
            // is the header that moves, not a folder: everything below an
            // account belongs to it, so the section travels whole.
            .on_drag(dragged, move |account, _, _, cx| {
                cx.new(|_| crate::message_list::DragGhost {
                    label: account.label.clone(),
                })
            })
            // A line on the edge the account will settle against, rather than
            // a filled row: a filled row is what dropping *mail into* a
            // folder looks like, and these two gestures live inches apart.
            .drag_over::<crate::workspace::DraggedAccount>({
                let order = order.clone();
                let onto = onto.clone();
                move |style, dragged, _, _| {
                    let from = order.iter().position(|e| e == dragged.email.as_ref());
                    let to = order.iter().position(|e| e == &onto);
                    match (from, to) {
                        (Some(from), Some(to)) if from < to => {
                            style.border_color(line).border_b_1()
                        }
                        (Some(_), Some(_)) => style.border_color(line).border_t_1(),
                        _ => style,
                    }
                }
            })
            .on_drop(cx.listener({
                let onto = onto.clone();
                move |this, dragged: &crate::workspace::DraggedAccount, _, cx| {
                    this.reorder_account(dragged, &onto, cx);
                }
            }))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .mt_2()
            .rounded_md()
            .when(ring == Some(&RowKey::Account(account_id)), |s| {
                s.bg(theme.selected)
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.sidebar_cursor = Some(RowKey::Account(account_id));
                this.select_scope(Scope::Account(account_id), cx)
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.open_folder_menu(MenuTarget::Account(account_id), event.position, cx);
                    cx.stop_propagation();
                }),
            )
            // The triangle, not the header, is what folds the account away:
            // clicking the name is how you select the account, and one click
            // cannot sensibly mean both.
            .child(
                twisty(
                    SharedString::from(format!("account-twisty-{}", account.id)),
                    collapsed,
                    theme,
                    cx.listener(move |this, _, _, cx| {
                        this.toggle_account_collapsed(&twisty_email, cx)
                    }),
                )
                .into_any_element(),
            )
            .child(div().size_2().rounded_full().bg(color).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .truncate()
                    .child(SharedString::from(account.name.clone())),
            )
            // Only while the account is folded away: its rows are the ones
            // wearing the spinner, and with them hidden the header is the only
            // place left to say a sync is inside this account. Shown on an
            // open account as well it would just be a second copy of what the
            // rows below are already saying.
            //
            // A failing connection follows the same rule, since it is the
            // inbox's mark — except on an account with no inbox to wear it,
            // which is what an account that has never got in looks like.
            .children(
                match corner(
                    collapsed && self.syncing_account(&email),
                    problem.clone().filter(|_| collapsed || !has_inbox),
                ) {
                    Corner::Syncing => Some(spinner(theme).into_any_element()),
                    Corner::Failing(reason) => Some(failing(reason, theme).into_any_element()),
                    Corner::Count => (hidden_unread > 0).then(|| {
                        div()
                            .flex_shrink_0()
                            .text_size(theme.small_text_size)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.count)
                            .child(format_count(hidden_unread as usize))
                            .into_any_element()
                    }),
                },
            )
            // Creating a folder belongs next to the account it goes into: the
            // menu item has to guess which account you meant, and this does
            // not. Faint until pointed at, because it is rare.
            .child(
                div()
                    .id(SharedString::from(format!("add-folder-{}", account.id)))
                    .flex_shrink_0()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(theme.text_faint)
                    .hover(|s| s.bg(theme.hover).text_color(theme.text))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_folder_prompt(
                            crate::folder_ui::FolderAction::Create {
                                account: account_id,
                            },
                            window,
                            cx,
                        );
                    }))
                    .child("+"),
            );

        let mut rows = Vec::new();
        if !collapsed {
            let context = FolderRows {
                email,
                color,
                ring: ring.cloned(),
                problem,
            };
            for node in &crate::folder_tree::build_sorted(folders) {
                self.push_folder_rows(&context, node, 0, &mut rows, theme, cx);
            }
        }

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex().flex_col().px_2().gap_px().children(rows))
            .into_any_element()
    }

    /// One folder row, then its children's, in the order they are drawn.
    ///
    /// Recursion rather than a flattening pass because the depth is what the
    /// indent is made of, and a collapsed node simply stops the walk — its
    /// children are not built at all, rather than built and hidden.
    fn push_folder_rows(
        &self,
        context: &FolderRows,
        node: &crate::folder_tree::Node<'_>,
        depth: usize,
        rows: &mut Vec<gpui::AnyElement>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) {
        let collapsed = self
            .settings
            .sidebar
            .folder_collapsed(&context.email, &node.path);
        let folded = node.has_children() && collapsed;

        let (unread, total) = node.counts(folded);

        let path = node.path.clone();
        let email = context.email.clone();
        let toggle =
            cx.listener(move |this, _, _, cx| this.toggle_folder_collapsed(&email, &path, cx));

        let disclosure = node.has_children().then(|| {
            twisty(
                SharedString::from(format!("twisty-{}-{}", context.email, node.path)),
                collapsed,
                theme,
                toggle,
            )
            .into_any_element()
        });

        // A container is not a folder: there is nothing to select, so the row
        // itself does the only thing it can, which is open and close.
        let id = node.id();
        let path = node.path.clone();
        let email = context.email.clone();
        let key = RowKey::Folder {
            email: context.email.clone(),
            path: node.path.clone(),
        };
        let ringed = context.ring.as_ref() == Some(&key);
        let on_click = cx.listener(move |this, _, _, cx| {
            this.sidebar_cursor = Some(key.clone());
            match id {
                Some(id) => this.select_mailbox(id, cx),
                None => this.toggle_folder_collapsed(&email, &path, cx),
            }
        });

        // A container has no folder of its own to sync, so it takes the state
        // of what is under it: `[Gmail]` spins while any of its children do.
        let syncing = self.syncing_folder(&context.email, &node.path)
            || node
                .children
                .iter()
                .any(|child| self.syncing_folder(&context.email, &child.path));

        // The inbox, because it is where new mail would have arrived: a
        // connection failing marks the one row whose count it has stopped,
        // not every folder the account has.
        let corner = corner(
            syncing,
            context.problem.clone().filter(|_| node.role() == MailboxRole::Inbox),
        );

        let mut row = folder_row(
            SharedString::from(format!("folder-{}-{}", context.email, node.path)),
            node.role().glyph(),
            node.name.clone().into(),
            unread,
            total,
            id.is_some_and(|id| self.scope == Scope::Mailbox(id)),
            corner,
            context.color,
            theme,
            depth,
            disclosure,
            on_click,
        );

        if let Some(id) = id {
            let drop_bg = theme.selected;
            let drop_border = theme.focus_ring;
            row = row
                // Every folder is a destination. The highlight is the only
                // thing telling you the drop will land, so it goes on the row
                // itself rather than on a container.
                .drag_over::<DraggedMessages>(move |style, _, _, _| {
                    style.bg(drop_bg).border_color(drop_border)
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedMessages, _, cx| {
                    this.drop_on_mailbox(dragged, id, cx);
                }))
                // Right-click acts on the row under the pointer, not on the
                // selection: renaming a folder should not first move you into
                // it and load a thousand messages you did not ask for.
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.open_folder_menu(MenuTarget::Folder(id), event.position, cx);
                        cx.stop_propagation();
                    }),
                );
        }

        // Like the message list's selection: full strength on the row the
        // keyboard is on while the sidebar has it, and the quieter
        // inactive shade on the open mailbox otherwise — so it is plain which
        // side the arrows will move.
        let open = id.is_some_and(|id| self.scope == Scope::Mailbox(id));
        if ringed {
            row = row.bg(theme.selected);
        } else if open {
            row = row.bg(theme.selected_inactive);
        }
        rows.push(row.into_any_element());

        if collapsed {
            return;
        }
        for child in &node.children {
            self.push_folder_rows(context, child, depth + 1, rows, theme, cx);
        }
    }
}

/// What every row in one account's folder list shares.
struct FolderRows {
    /// The account's address, which is how a collapsed row is remembered.
    email: String,
    color: Hsla,
    /// The row the keyboard is on, while the sidebar has it.
    ring: Option<RowKey>,
    /// What is wrong with the account's connection, for its inbox to show.
    problem: Option<SharedString>,
}

/// Where a dragged row ends up: `moved` taken out and put back where `onto`
/// is, from whichever direction it came.
///
/// Dropping a row from above onto another puts it below that one, and from
/// below puts it above — the arrangement the pointer was pointing at, rather
/// than one that is right half the time.
pub fn reorder(rows: &[String], moved: &str, onto: &str) -> Vec<String> {
    let mut out: Vec<String> = rows.to_vec();
    let (Some(from), Some(to)) = (
        out.iter().position(|r| r == moved),
        out.iter().position(|r| r == onto),
    ) else {
        return out;
    };
    if from == to {
        return out;
    }

    let row = out.remove(from);
    let at = out.iter().position(|r| r == onto).unwrap_or(out.len());
    let at = match from < to {
        true => at + 1,
        false => at,
    };
    out.insert(at.min(out.len()), row);
    out
}

/// The disclosure triangle, and the only control in the sidebar that folds
/// anything away.
///
/// Its own hit area rather than the whole row's: the row already means "go
/// here", and a click that means both that and "open this" would be a click
/// nobody could aim.
fn twisty(
    id: impl Into<gpui::ElementId>,
    collapsed: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(TWISTY_WIDTH))
        .flex_shrink_0()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme.text_faint)
        .hover(|s| s.text_color(theme.text))
        .on_click(move |event, window, cx| {
            // Without this the row underneath also fires, and opening a
            // folder's children would move you into it.
            cx.stop_propagation();
            on_click(event, window, cx);
        })
        .child(if collapsed { "\u{25B8}" } else { "\u{25BE}" })
}

/// The width of the disclosure column, which every row reserves whether or not
/// it has a triangle — otherwise a folder's glyph sits in a different place
/// depending on whether its neighbour has children.
const TWISTY_WIDTH: f32 = 14.;

/// How far one level of nesting moves a row to the right.
///
/// Enough to read as a level, not so much that four levels of a Dovecot
/// account run out of sidebar to be indented in.
const INDENT: f32 = 11.;

/// The mark a row wears while it is being synced.
///
/// A dot that breathes rather than a spinning glyph: it occupies exactly the
/// space the count did, so no row changes width when a sync starts, and there
/// is no font on the machine whose spinner characters are all the same width.
/// It says only "something is happening here", which is all this corner of the
/// row can honestly say mid-fetch.
fn spinner(theme: &Theme) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(14.))
        .child(
            div()
                .size(px(6.))
                .rounded_full()
                .bg(theme.accent)
                .with_animation(
                    "folder-sync",
                    Animation::new(Duration::from_millis(900))
                        .repeat()
                        .with_easing(ease_in_out),
                    |dot, delta| {
                        // Fades down and back rather than to nothing: a dot that
                        // vanishes reads as a row losing its badge.
                        dot.opacity(0.35 + 0.65 * (delta * std::f32::consts::TAU).sin().abs())
                    },
                ),
        )
}

/// What the right-hand corner of a row shows.
#[derive(Debug, Clone, PartialEq)]
enum Corner {
    /// Its count, which is what it shows when there is nothing to say.
    Count,
    /// A sync is inside it right now. The number is about to change, and what
    /// that corner is asked mid-fetch is "is anything happening", not "how
    /// many were there a moment ago".
    Syncing,
    /// The account's connection is failing, for this reason. The count stays
    /// hidden for the same reason it does mid-sync: it is no longer the
    /// number on the server, and nothing is coming to change it.
    Failing(SharedString),
}

/// Which mark a row wears, given what is true of it.
///
/// A sync beats a failure: a sync running on a failing account is somebody
/// trying again, and the dot breathing says the try is under way. If it
/// fails too, the red comes back when it ends.
fn corner(syncing: bool, problem: Option<SharedString>) -> Corner {
    match (syncing, problem) {
        (true, _) => Corner::Syncing,
        (false, Some(reason)) => Corner::Failing(reason),
        (false, None) => Corner::Count,
    }
}

/// The mark a row wears while its account cannot be reached or logged in to.
///
/// The spinner's dot, held still and in red: the same place and the same
/// size, so it reads as the same signal gone wrong, and in the one colour this
/// sidebar otherwise never uses. The reason is on hover, since a sentence in a
/// 200-point column is a sentence nobody can read — the dot is what has to be
/// noticed.
fn failing(reason: SharedString, theme: &Theme) -> impl IntoElement {
    div()
        .id("connection-failing")
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(14.))
        .h(px(14.))
        .tooltip(move |_, cx| {
            cx.new(|_| crate::chrome::Tip {
                text: reason.clone(),
            })
            .into()
        })
        .child(div().size(px(6.)).rounded_full().bg(theme.red))
}

/// One selectable row. Shared by the unified view and every folder so their
/// selected and hover states cannot drift apart.
#[allow(clippy::too_many_arguments)]
fn folder_row(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    name: SharedString,
    unread: u32,
    total: u32,
    selected: bool,
    // Whether the count gives way to a mark: see `Corner`.
    corner: Corner,
    accent: Hsla,
    theme: &Theme,
    // `depth` is how deep in the tree the row sits, which is its indent;
    // `disclosure` is the triangle, for a row with children to fold away.
    depth: usize,
    disclosure: Option<gpui::AnyElement>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let text_color = if selected {
        theme.text
    } else {
        theme.text_muted
    };

    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1p5()
        .px_2()
        .pl(px(8. + depth as f32 * INDENT))
        .py_1()
        .rounded_md()
        .cursor_pointer()
        // Transparent rather than absent: a border that only exists while
        // something is dragged over would shift the row by a pixel.
        .border_1()
        .border_color(gpui::transparent_black())
        .when(selected, |s| s.bg(theme.selected))
        .when(!selected, |s| s.hover(|s| s.bg(theme.hover)))
        .on_click(on_click)
        // The triangle's column is reserved either way, so a folder with
        // children and one without still line their names up.
        .child(match disclosure {
            Some(triangle) => triangle,
            None => div().w(px(TWISTY_WIDTH)).flex_shrink_0().into_any_element(),
        })
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .text_color(if selected { accent } else { theme.text_faint })
                .child(glyph),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(text_color)
                .when(unread > 0, |s| {
                    s.font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                })
                .child(name),
        )
        .child(match corner {
            Corner::Syncing => spinner(theme).into_any_element(),
            Corner::Failing(reason) => failing(reason, theme).into_any_element(),
            // Unread is the number you act on, so it gets weight and the
            // brightest text the palette has; the total is there only for
            // orientation. The account's own color stays on the dot and the
            // glyph — an unread count rendered in red would read as an alarm.
            Corner::Count => div()
                .flex_shrink_0()
                .text_size(theme.small_text_size)
                .when(unread > 0, |s| {
                    s.text_color(theme.count)
                        .font_weight(gpui::FontWeight::MEDIUM)
                })
                .when(unread == 0, |s| s.text_color(theme.text_faint))
                .child(if unread > 0 {
                    format_count(unread as usize)
                } else if total > 0 {
                    format_count(total as usize)
                } else {
                    String::new()
                })
                .into_any_element(),
        })
}

#[cfg(test)]
mod tests {
    use super::{Corner, corner, reorder};

    #[test]
    fn a_failing_connection_takes_the_count_and_a_sync_takes_it_back() {
        let reason = || Some("Not syncing: timed out".into());

        assert_eq!(corner(false, None), Corner::Count);
        assert_eq!(corner(true, None), Corner::Syncing);
        assert_eq!(
            corner(false, reason()),
            Corner::Failing("Not syncing: timed out".into())
        );
        // Somebody pressed Get New Mail: the try shows as a try.
        assert_eq!(corner(true, reason()), Corner::Syncing);
    }

    fn rows(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn an_account_lands_on_the_side_it_was_dragged_from() {
        let accounts = rows(&["a@x", "b@x", "c@x", "d@x"]);

        // Dragged down onto `c`: it settles below the row it was dropped on.
        assert_eq!(
            reorder(&accounts, "a@x", "c@x"),
            ["b@x", "c@x", "a@x", "d@x"]
        );
        // Dragged up onto `b`: above it.
        assert_eq!(
            reorder(&accounts, "d@x", "b@x"),
            ["a@x", "d@x", "b@x", "c@x"]
        );
    }

    #[test]
    fn a_drop_that_changes_nothing_changes_nothing() {
        let accounts = rows(&["a@x", "b@x"]);
        assert_eq!(reorder(&accounts, "a@x", "a@x"), accounts, "onto itself");
        assert_eq!(
            reorder(&accounts, "a@x", "gone@x"),
            accounts,
            "onto a stranger"
        );
        assert_eq!(
            reorder(&accounts, "gone@x", "a@x"),
            accounts,
            "from a stranger"
        );
    }

    #[test]
    fn either_end_is_reachable() {
        let accounts = rows(&["a@x", "b@x", "c@x"]);
        assert_eq!(
            reorder(&accounts, "c@x", "a@x"),
            ["c@x", "a@x", "b@x"],
            "the top"
        );
        assert_eq!(
            reorder(&accounts, "a@x", "c@x"),
            ["b@x", "c@x", "a@x"],
            "the bottom"
        );
    }
}
