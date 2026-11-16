//! Creating, renaming and deleting IMAP mailboxes.
//!
//! Every one of these is a change on someone else's server, so they all follow
//! the same shape: ask, do it on the server, and only then touch the local
//! store. Writing locally first and hoping the server agrees is how a client
//! ends up showing a folder that does not exist.
//!
//! The delete is the one that deserves care — it destroys server-side mail —
//! so it names the mailbox, says how many messages go with it, and refuses
//! outright for the special-use folders every account needs.

use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px};
use spool_core::{AccountId, MailboxId, MailboxRole};
use spool_net::{Config, FolderOp};
use spool_store::Scope;

use crate::{
    activity::ActivityKind, text_input::TextInput, theme::Theme, workspace::Workspace,
};

/// What the prompt on screen is about to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderAction {
    Create { account: AccountId },
    Rename { mailbox: MailboxId },
    Delete { mailbox: MailboxId },
}

pub struct FolderPrompt {
    pub action: FolderAction,
    /// The name or path being typed. Absent for a delete, which asks a
    /// question rather than taking a value.
    pub input: Option<Entity<TextInput>>,
    /// This account's hierarchy separator, for the placeholder and for the
    /// local rename.
    pub delimiter: String,
    pub busy: bool,
    pub error: Option<String>,
    _subscription: Option<gpui::Subscription>,
}

impl Workspace {
    /// The folder the sidebar is currently showing, which is what the rename
    /// and delete commands act on.
    pub fn selected_mailbox(&self) -> Option<MailboxId> {
        match self.scope {
            Scope::Mailbox(id) => Some(id),
            _ => None,
        }
    }

    /// The account a new folder would belong to: the one being looked at, or
    /// the only one there is.
    pub fn current_account(&self) -> Option<AccountId> {
        match self.scope {
            Scope::Account(id) => Some(id),
            Scope::Mailbox(id) => {
                self.mailboxes.iter().find(|m| m.id == id).map(|m| m.account_id)
            }
            Scope::Everything => match self.accounts.as_slice() {
                [only] => Some(only.id),
                _ => None,
            },
        }
    }

    pub fn open_folder_prompt(
        &mut self,
        action: FolderAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account = match action {
            FolderAction::Create { account } => account,
            FolderAction::Rename { mailbox } | FolderAction::Delete { mailbox } => {
                match self.mailboxes.iter().find(|m| m.id == mailbox) {
                    Some(folder) => folder.account_id,
                    None => return,
                }
            }
        };
        let delimiter = self.delimiter_for(account);

        // The special-use folders are load-bearing: sync files mail into them
        // by role, and the server will refuse anyway. Saying so up front beats
        // relaying a numeric error from the other end.
        if let FolderAction::Rename { mailbox } | FolderAction::Delete { mailbox } = action
            && let Some(folder) = self.mailboxes.iter().find(|m| m.id == mailbox)
            && folder.role != MailboxRole::Other
        {
            self.status = format!("{} is a special folder and cannot be changed", folder.name);
            cx.notify();
            return;
        }

        let input = match action {
            FolderAction::Delete { .. } => None,
            _ => {
                let placeholder = match action {
                    FolderAction::Create { .. } => {
                        format!("Folder name \u{2014} {delimiter} to nest it")
                    }
                    _ => "New path".to_string(),
                };
                let existing = match action {
                    FolderAction::Rename { mailbox } => self
                        .mailboxes
                        .iter()
                        .find(|m| m.id == mailbox)
                        .map(|m| m.path.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                };

                Some(cx.new(|cx| {
                    let mut input = TextInput::new(placeholder, cx);
                    // A rename starts from the whole path, so the folder can
                    // be moved as well as renamed — which is the same IMAP
                    // command either way.
                    input.set_content(existing, cx);
                    input
                }))
            }
        };

        if let Some(input) = input.as_ref() {
            let handle = input.read(cx).focus_handle.clone();
            window.focus(&handle);
        }

        self.folder_prompt = Some(FolderPrompt {
            action,
            input,
            delimiter,
            busy: false,
            error: None,
            _subscription: None,
        });
        cx.notify();
    }

    pub fn close_folder_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.folder_prompt = None;
        window.focus(&self.list_focus);
        cx.notify();
    }

    /// The hierarchy separator this account uses, read off the paths sync
    /// already brought back.
    ///
    /// It varies — `/` on Gmail, `.` on Dovecot's default layout — and the
    /// authority is the server's `LIST` response, which is where these paths
    /// came from. Guessing from what is already on disk avoids a round trip
    /// before the prompt can even be drawn.
    pub fn delimiter_for(&self, account: AccountId) -> String {
        let paths = self.mailboxes.iter().filter(|m| m.account_id == account);
        for mailbox in paths {
            if mailbox.path.contains('/') {
                return "/".into();
            }
            if mailbox.path.contains('.') {
                return ".".into();
            }
        }
        "/".into()
    }

    /// Runs whatever the prompt is asking about.
    pub fn confirm_folder_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.folder_prompt.as_ref() else { return };
        if prompt.busy {
            return;
        }

        let action = prompt.action;
        let delimiter = prompt.delimiter.clone();
        let typed = prompt
            .input
            .as_ref()
            .map(|input| input.read(cx).text().trim().to_string())
            .unwrap_or_default();

        let (account_id, op, target) = match self.plan(action, &typed, &delimiter) {
            Ok(plan) => plan,
            Err(problem) => {
                if let Some(prompt) = self.folder_prompt.as_mut() {
                    prompt.error = Some(problem);
                }
                cx.notify();
                return;
            }
        };

        let Some(account) = self.accounts.iter().find(|a| a.id == account_id) else { return };
        let email = account.email.clone();

        if let Some(prompt) = self.folder_prompt.as_mut() {
            prompt.busy = true;
            prompt.error = None;
        }
        self.status = op.describing();
        self.activities.begin(ActivityKind::Folder, op.describing());
        cx.notify();

        let store = self.store.clone();
        self.folder_task = Some(cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn({
                    let op = op.clone();
                    async move { run(&email, &op).await }
                })
                .await;

            this.update(cx, |this, cx| {
                this.activities.end(ActivityKind::Folder);

                match done {
                    Ok(()) => {
                        // Only now is the local copy allowed to change: the
                        // server is the thing that decides what folders exist.
                        let outcome = match (&op, target) {
                            (FolderOp::Create { path }, _) => store
                                .upsert_mailbox(account_id, path, MailboxRole::Other)
                                .map(|id| {
                                    this.scope = Scope::Mailbox(id);
                                    format!("Created {path}")
                                }),
                            (FolderOp::Rename { to, .. }, Some(mailbox)) => store
                                .rename_mailbox(mailbox, to, &delimiter)
                                .map(|moved| match moved {
                                    1 => format!("Renamed to {to}"),
                                    n => format!("Renamed to {to} and {} inside it", n - 1),
                                }),
                            (FolderOp::Delete { path }, Some(mailbox)) => {
                                // Standing in a folder that no longer exists
                                // would leave an empty list with a live title.
                                if this.scope == Scope::Mailbox(mailbox) {
                                    this.scope = Scope::Everything;
                                }
                                store.delete_mailbox(mailbox).map(|count| match count {
                                    0 => format!("Deleted {path}"),
                                    n => format!("Deleted {path} and {n} messages"),
                                })
                            }
                            _ => Ok(String::new()),
                        };

                        this.folder_prompt = None;
                        this.set_sticky_status(match outcome {
                            Ok(message) => message,
                            // The server did it; only the local copy is stale,
                            // and the next sync reconciles it.
                            Err(e) => format!("Done on the server, but: {e:#}"),
                        });
                        this.refresh_folders(cx);
                        this.reload(cx);
                    }
                    Err(e) => {
                        if let Some(prompt) = this.folder_prompt.as_mut() {
                            prompt.busy = false;
                            prompt.error = Some(format!("{e:#}"));
                        }
                        this.status = String::new();
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Turns what was typed into the operation to run, or says why it cannot.
    fn plan(
        &self,
        action: FolderAction,
        typed: &str,
        delimiter: &str,
    ) -> Result<(AccountId, FolderOp, Option<MailboxId>), String> {
        match action {
            FolderAction::Create { account } => {
                let path = clean_path(typed, delimiter)?;
                if self
                    .mailboxes
                    .iter()
                    .any(|m| m.account_id == account && m.path.eq_ignore_ascii_case(&path))
                {
                    return Err(format!("{path} already exists"));
                }
                Ok((account, FolderOp::Create { path }, None))
            }
            FolderAction::Rename { mailbox } => {
                let folder =
                    self.mailboxes.iter().find(|m| m.id == mailbox).ok_or("no such folder")?;
                let path = clean_path(typed, delimiter)?;
                if path == folder.path {
                    return Err("That is the name it already has".into());
                }
                Ok((
                    folder.account_id,
                    FolderOp::Rename { from: folder.path.clone(), to: path },
                    Some(mailbox),
                ))
            }
            FolderAction::Delete { mailbox } => {
                let folder =
                    self.mailboxes.iter().find(|m| m.id == mailbox).ok_or("no such folder")?;
                Ok((
                    folder.account_id,
                    FolderOp::Delete { path: folder.path.clone() },
                    Some(mailbox),
                ))
            }
        }
    }

    pub(crate) fn render_folder_prompt(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let prompt = self.folder_prompt.as_ref()?;

        let (title, detail, confirm) = match prompt.action {
            FolderAction::Create { .. } => {
                ("New folder".to_string(), String::new(), "Create")
            }
            FolderAction::Rename { mailbox } => {
                let name = self
                    .mailboxes
                    .iter()
                    .find(|m| m.id == mailbox)
                    .map(|m| m.path.clone())
                    .unwrap_or_default();
                (format!("Rename {name}"), String::new(), "Rename")
            }
            FolderAction::Delete { mailbox } => {
                let folder = self.mailboxes.iter().find(|m| m.id == mailbox);
                let name = folder.map(|m| m.path.clone()).unwrap_or_default();
                let held = folder.map(|m| m.total).unwrap_or(0);
                (
                    format!("Delete {name}?"),
                    match held {
                        0 => "The folder is empty. This cannot be undone.".to_string(),
                        1 => "The message in it is deleted from the server too. \
                              This cannot be undone."
                            .to_string(),
                        n => format!(
                            "All {n} messages in it are deleted from the server too. \
                             This cannot be undone."
                        ),
                    },
                    "Delete",
                )
            }
        };

        let destructive = matches!(prompt.action, FolderAction::Delete { .. });
        let busy = prompt.busy;

        Some(
            div()
                .id("folder-prompt-scrim")
                .absolute()
                .inset_0()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_folder_prompt(window, cx);
                }))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(140.))
                .bg(theme.scrim)
                .child(
                    div()
                        .id("folder-prompt")
                        .w(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.border)
                        // The modal swallows its own clicks so the scrim does
                        // not close it out from under the caret.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                        .children((!detail.is_empty()).then(|| {
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(if destructive {
                                    theme.red
                                } else {
                                    theme.text_muted
                                })
                                .child(SharedString::from(detail))
                        }))
                        .children(prompt.input.as_ref().map(|input| {
                            div()
                                .px_2p5()
                                .py_1p5()
                                .rounded_md()
                                .bg(theme.background)
                                .border_1()
                                .border_color(theme.focus_ring)
                                .child(input.clone())
                        }))
                        .children(prompt.error.as_ref().map(|error| {
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(theme.red)
                                .child(SharedString::from(error.clone()))
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .text_size(theme.small_text_size)
                                        .text_color(theme.text_faint)
                                        .child(if busy {
                                            "Talking to the server\u{2026}"
                                        } else {
                                            "enter to confirm \u{00b7} escape to cancel"
                                        }),
                                )
                                .child(
                                    div()
                                        .id("folder-prompt-confirm")
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .bg(if destructive { theme.red } else { theme.accent })
                                        .text_color(theme.panel)
                                        .when(busy, |s| s.opacity(0.5))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_folder_prompt(cx);
                                        }))
                                        .child(confirm),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// Connects as the account that owns the folder and runs the operation.
async fn run(email: &str, op: &FolderOp) -> anyhow::Result<()> {
    let config = Config::load(Config::default_path())?;
    let account = config
        .accounts
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(email))
        .ok_or_else(|| anyhow::anyhow!("no server configured for {email}"))?;

    let credentials = account.credentials().await?;
    spool_net::folder_op(&account, &credentials, op).await
}

/// Checks a typed folder path, and says what is wrong with it if anything is.
///
/// Deliberately permissive about what a folder may be *called* — servers
/// disagree, and the one holding the mailbox is the authority — but strict
/// about the shape of the path, because an empty segment is the one mistake
/// that produces a folder nobody can select or delete.
fn clean_path(typed: &str, delimiter: &str) -> Result<String, String> {
    let path = typed.trim();
    if path.is_empty() {
        return Err("A folder needs a name".into());
    }
    if path.contains(['\r', '\n', '"']) {
        return Err("A folder name cannot contain quotes or line breaks".into());
    }
    if !delimiter.is_empty() {
        let trimmed = path.trim_matches(|c: char| delimiter.starts_with(c));
        if trimmed.is_empty() || path.contains(&format!("{delimiter}{delimiter}")) {
            return Err(format!("{delimiter} separates folder names; it cannot stand alone"));
        }
        if trimmed != path {
            return Err(format!("A folder name cannot start or end with {delimiter}"));
        }
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::clean_path;

    #[test]
    fn an_ordinary_name_is_left_alone() {
        assert_eq!(clean_path("Invoices", "/").unwrap(), "Invoices");
        assert_eq!(clean_path("  Invoices  ", "/").unwrap(), "Invoices");
        // Nesting is just the delimiter, typed.
        assert_eq!(clean_path("Work/Invoices", "/").unwrap(), "Work/Invoices");
        assert_eq!(clean_path("INBOX.Work", ".").unwrap(), "INBOX.Work");
    }

    #[test]
    fn an_empty_segment_is_refused_rather_than_created() {
        // `Work//Invoices` and `/Work` are the paths that produce a folder
        // nobody can select or delete afterwards.
        assert!(clean_path("", "/").is_err());
        assert!(clean_path("   ", "/").is_err());
        assert!(clean_path("/", "/").is_err());
        assert!(clean_path("/Work", "/").is_err());
        assert!(clean_path("Work/", "/").is_err());
        assert!(clean_path("Work//Invoices", "/").is_err());
    }

    #[test]
    fn quotes_and_line_breaks_are_refused() {
        // Both would have to be escaped into the IMAP command, and neither is
        // something a person meant to type into a folder name.
        assert!(clean_path("Say \"hi\"", "/").is_err());
        assert!(clean_path("Work\r\nDELETE", "/").is_err());
    }

    #[test]
    fn a_name_containing_the_other_delimiter_is_fine() {
        // On a server that separates with `.`, a slash is an ordinary
        // character in a folder name, and vice versa.
        assert_eq!(clean_path("and/or", ".").unwrap(), "and/or");
        assert_eq!(clean_path("v1.2", "/").unwrap(), "v1.2");
    }
}
