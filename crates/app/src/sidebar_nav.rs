//! Moving through the sidebar from the keyboard, the way Finder's list view
//! moves through folders.
//!
//! Up and down walk the rows as they are drawn, opening each as it is reached
//! — the way Mail's sidebar does, since a mailbox is only worth landing on to
//! read. Right opens a folded row, or steps into an open one; left folds an
//! open row, or steps out to the one it sits under. Return, or right on a
//! folder with nothing under it, hands the keyboard to the message list; left
//! in the list hands it back.
//!
//! The rows are worked out from the same state the sidebar is drawn from, so
//! what the keys walk is exactly what is on screen: a folded account's folders
//! are not there to be walked into.

use gpui::{Context, Window};
use spool_core::AccountId;
use spool_store::Scope;

use crate::{
    actions::{FocusMessageList, FocusSidebar, SidebarCollapse, SidebarExpand, SidebarNext, SidebarPrev},
    workspace::Workspace,
};

/// Which row, in terms that survive the rows being worked out again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowKey {
    AllMail,
    Account(AccountId),
    Folder { email: String, path: String },
}

/// One row of the sidebar, as the keyboard sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavRow {
    pub key: RowKey,
    /// How far in: accounts at 0, their top folders at 1.
    pub depth: usize,
    /// `Some(folded)` for a row with rows under it, `None` for one without.
    pub children: Option<bool>,
    /// What landing on it shows. `None` for a container, which holds folders
    /// but no mail of its own.
    pub opens: Option<Scope>,
}

/// The row a row sits under: the nearest one above it that is less deep.
pub(crate) fn parent(rows: &[NavRow], index: usize) -> Option<usize> {
    let depth = rows.get(index)?.depth;
    rows[..index].iter().rposition(|row| row.depth < depth)
}

/// The first row under an open row, if it has one.
pub(crate) fn first_child(rows: &[NavRow], index: usize) -> Option<usize> {
    let depth = rows.get(index)?.depth;
    rows.get(index + 1).filter(|next| next.depth > depth).map(|_| index + 1)
}

impl Workspace {
    /// Every row of the sidebar, top to bottom, as drawn.
    pub(crate) fn sidebar_rows(&self) -> Vec<NavRow> {
        let mut rows = Vec::new();
        if self.settings.unified_inbox {
            rows.push(NavRow { key: RowKey::AllMail, depth: 0, children: None, opens: Some(Scope::Everything) });
        }
        for account in self.sorted_accounts() {
            let folders: Vec<_> = self.mailboxes.iter().filter(|m| m.account_id == account.id).collect();
            let folded = self.settings.sidebar.account_collapsed(&account.email);
            rows.push(NavRow {
                key: RowKey::Account(account.id),
                depth: 0,
                children: (!folders.is_empty()).then_some(folded),
                opens: Some(Scope::Account(account.id)),
            });
            if !folded {
                for node in &crate::folder_tree::build_sorted(&folders) {
                    self.push_nav_rows(&account.email, node, 1, &mut rows);
                }
            }
        }
        rows
    }

    fn push_nav_rows(&self, email: &str, node: &crate::folder_tree::Node<'_>, depth: usize, rows: &mut Vec<NavRow>) {
        let folded = self.settings.sidebar.folder_collapsed(email, &node.path);
        rows.push(NavRow {
            key: RowKey::Folder { email: email.to_string(), path: node.path.clone() },
            depth,
            children: node.has_children().then_some(folded),
            opens: node.id().map(Scope::Mailbox),
        });
        if node.has_children() && !folded {
            for child in &node.children {
                self.push_nav_rows(email, child, depth + 1, rows);
            }
        }
    }

    /// The row the keyboard is on: the one last moved to or clicked while it
    /// is still on screen, else the one showing what the list is showing.
    pub(crate) fn sidebar_cursor_index(&self, rows: &[NavRow]) -> Option<usize> {
        self.sidebar_cursor
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key))
            .or_else(|| rows.iter().position(|row| row.opens == Some(self.scope)))
    }

    /// The row to draw the keyboard's ring on, while the sidebar has it.
    pub(crate) fn sidebar_ring(&self, window: &Window) -> Option<RowKey> {
        if !self.sidebar_focus.is_focused(window) {
            return None;
        }
        let rows = self.sidebar_rows();
        self.sidebar_cursor_index(&rows).map(|index| rows[index].key.clone())
    }

    /// Puts the keyboard on a row and shows what it holds.
    fn land_on(&mut self, row: &NavRow, cx: &mut Context<Self>) {
        self.sidebar_cursor = Some(row.key.clone());
        match row.opens {
            Some(scope) if scope != self.scope => match scope {
                Scope::Mailbox(id) => self.select_mailbox(id, cx),
                other => self.select_scope(other, cx),
            },
            _ => cx.notify(),
        }
    }

    fn fold(&mut self, row: &NavRow, cx: &mut Context<Self>) {
        match &row.key {
            RowKey::Account(id) => {
                let email = self.accounts.iter().find(|a| a.id == *id).map(|a| a.email.clone());
                if let Some(email) = email {
                    self.toggle_account_collapsed(&email, cx);
                }
            }
            RowKey::Folder { email, path } => {
                let (email, path) = (email.clone(), path.clone());
                self.toggle_folder_collapsed(&email, &path, cx);
            }
            RowKey::AllMail => {}
        }
    }

    pub(crate) fn focus_sidebar(&mut self, _: &FocusSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar_visible {
            return;
        }
        // Onto what the list is showing, not wherever the keys last left it.
        self.sidebar_cursor = None;
        window.focus(&self.sidebar_focus);
        cx.notify();
    }

    pub(crate) fn focus_message_list(&mut self, _: &FocusMessageList, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.list_focus);
        cx.notify();
    }

    pub(crate) fn sidebar_next(&mut self, _: &SidebarNext, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_step(1, cx);
    }

    pub(crate) fn sidebar_prev(&mut self, _: &SidebarPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_step(-1, cx);
    }

    fn sidebar_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.sidebar_rows();
        if rows.is_empty() {
            return;
        }
        let next = match self.sidebar_cursor_index(&rows) {
            // Stops at the ends rather than wrapping, as a list does.
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        let row = rows[next].clone();
        self.land_on(&row, cx);
    }

    pub(crate) fn sidebar_expand(&mut self, _: &SidebarExpand, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.sidebar_rows();
        let Some(index) = self.sidebar_cursor_index(&rows) else { return };
        let row = rows[index].clone();
        self.sidebar_cursor = Some(row.key.clone());
        match row.children {
            Some(true) => self.fold(&row, cx),
            Some(false) => {
                if let Some(child) = first_child(&rows, index) {
                    let child = rows[child].clone();
                    self.land_on(&child, cx);
                }
            }
            // Nothing further in: the next thing in is its mail.
            None if row.opens.is_some() => {
                window.focus(&self.list_focus);
                cx.notify();
            }
            None => {}
        }
    }

    pub(crate) fn sidebar_collapse(&mut self, _: &SidebarCollapse, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.sidebar_rows();
        let Some(index) = self.sidebar_cursor_index(&rows) else { return };
        let row = rows[index].clone();
        self.sidebar_cursor = Some(row.key.clone());
        match (row.children, parent(&rows, index)) {
            (Some(false), _) => self.fold(&row, cx),
            (_, Some(up)) => {
                let up = rows[up].clone();
                self.land_on(&up, cx);
            }
            _ => cx.notify(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(depth: usize, children: Option<bool>) -> NavRow {
        NavRow { key: RowKey::AllMail, depth, children, opens: None }
    }

    /// An account, a folder with two under it, and a second account.
    fn tree() -> Vec<NavRow> {
        vec![row(0, Some(false)), row(1, Some(false)), row(2, None), row(2, None), row(1, None), row(0, None)]
    }

    #[test]
    fn a_row_steps_out_to_the_one_it_sits_under() {
        let rows = tree();
        assert_eq!(parent(&rows, 3), Some(1), "past its sibling, to the folder above");
        assert_eq!(parent(&rows, 1), Some(0), "a top folder, to its account");
        assert_eq!(parent(&rows, 4), Some(0));
        assert_eq!(parent(&rows, 0), None, "an account is as far out as it goes");
        assert_eq!(parent(&rows, 9), None);
    }

    #[test]
    fn an_open_row_steps_into_its_first_child_and_a_leaf_does_not() {
        let rows = tree();
        assert_eq!(first_child(&rows, 0), Some(1));
        assert_eq!(first_child(&rows, 1), Some(2));
        assert_eq!(first_child(&rows, 2), None, "its sibling is not its child");
        assert_eq!(first_child(&rows, 4), None, "the next account is not its child");
        assert_eq!(first_child(&rows, 5), None, "nothing after the last row");
    }
}
