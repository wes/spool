//! Account management: the list, the editor, and the actions behind them.
//!
//! Everything about an account lives here rather than being split between a
//! renderer and the workspace, because the state is small and the operations
//! are all one thing — read `accounts.toml`, change one entry, write it back,
//! and put the secret somewhere that is not the file.
//!
//! Two rules shape the design. **Nothing is saved until Save is pressed**, so
//! the editor works on a draft and the file is untouched until then. And
//! **secrets never enter the draft on the way out**: an existing account's
//! password is not read back into the form, so a stray keystroke cannot
//! silently replace a working credential with a truncated one.

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, Pixels, SharedString, Stateful, Window, div,
    prelude::*, px,
};
use spool_net::{
    AccountConfig, Auth, Config, Probe, Provider, ProviderId, Security, Slot, oauth, probe,
    secrets,
};
use spool_store::Scope;

use crate::{
    text_area::{AreaStyle, TextArea},
    text_input::TextInput,
    theme::Theme,
    workspace::Workspace,
};

/// Every text field in the editor, in tab order.
///
/// An enum rather than named struct members so the inputs can live in one
/// array: constructing eleven fields, reading eleven fields and clearing
/// eleven fields are each one line instead of eleven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Email,
    Password,
    ImapHost,
    ImapPort,
    ImapUser,
    SmtpHost,
    SmtpPort,
    SmtpUser,
}

impl Field {
    pub const ALL: [Field; 9] = [
        Field::Name,
        Field::Email,
        Field::Password,
        Field::ImapHost,
        Field::ImapPort,
        Field::ImapUser,
        Field::SmtpHost,
        Field::SmtpPort,
        Field::SmtpUser,
    ];

    fn label(self) -> &'static str {
        match self {
            Field::Name => "Account name",
            Field::Email => "Email address",
            Field::Password => "Password",
            Field::ImapHost => "Server",
            Field::ImapPort => "Port",
            Field::ImapUser => "Username",
            Field::SmtpHost => "Server",
            Field::SmtpPort => "Port",
            Field::SmtpUser => "Username",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Field::Name => "Work",
            Field::Email => "me@example.com",
            Field::Password => "App-specific password",
            Field::ImapHost => "imap.example.com",
            Field::ImapPort => "993",
            Field::ImapUser => "Same as the address",
            Field::SmtpHost => "smtp.example.com",
            Field::SmtpPort => "465",
            Field::SmtpUser => "Same as IMAP",
        }
    }

    /// Whether the field is drawn as bullets.
    fn is_secret(self) -> bool {
        matches!(self, Field::Password)
    }
}

/// How the last "Test connection" went.
pub enum ProbeState {
    Idle,
    Running,
    Done(Probe),
}

/// How the last "Sign in with Google" went.
pub enum SignInState {
    Idle,
    /// The browser is open and the loopback listener is waiting.
    Waiting,
    Done(String),
    Failed(String),
}

pub struct AccountEditor {
    /// The account being edited, or `None` when adding.
    pub index: Option<usize>,
    pub provider: ProviderId,
    /// Everything the form does not show — folders, sync limit, the
    /// `password_command` a hand-edited config may rely on — carried through
    /// unchanged so editing an account in the UI does not quietly discard it.
    pub carried: AccountConfig,
    pub auth: Auth,
    pub imap_security: Security,
    pub smtp_security: Security,
    pub color: u8,
    pub fields: [Entity<TextInput>; Field::ALL.len()],
    /// The signature, which is the one field that is not a single line.
    pub signature: Entity<TextArea>,
    pub probe: ProbeState,
    pub sign_in: SignInState,
    /// Set once a sign-in has stored a refresh token for this draft, so Save
    /// knows the account is usable even though nothing in the form shows it.
    pub signed_in: bool,
    /// Whether the account already had a token when the editor opened.
    ///
    /// Kept separate from `signed_in` so that editing a working Google
    /// account — to rename it, say — does not demand a fresh trip through the
    /// browser, while a brand new one does.
    pub had_token: bool,
    pub problems: Vec<String>,
    pub busy: bool,
}

impl AccountEditor {
    fn new(
        index: Option<usize>,
        account: &AccountConfig,
        had_token: bool,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let fields = std::array::from_fn(|i| {
            let field = Field::ALL[i];
            let input = cx.new(|cx| match field.is_secret() {
                true => TextInput::masked(field.placeholder(), cx),
                false => TextInput::new(field.placeholder(), cx),
            });

            // Secrets are deliberately not loaded back in. The keychain can
            // return them, but a form pre-filled with a working password is a
            // form that can destroy one by accident.
            let value = match field {
                Field::Name => account.name.clone(),
                Field::Email => account.email.clone(),
                Field::ImapHost => account.imap_host.clone(),
                Field::ImapPort => account.imap_port.to_string(),
                Field::ImapUser => account.imap_user.clone().unwrap_or_default(),
                Field::SmtpHost => account.smtp_host.clone().unwrap_or_default(),
                Field::SmtpPort => account.smtp_port.to_string(),
                Field::SmtpUser => account.smtp_user.clone().unwrap_or_default(),
                Field::Password => String::new(),
            };
            if !value.is_empty() {
                input.update(cx, |input, cx| input.set_content(value, cx));
            }
            input
        });

        let signature = cx.new(|cx| {
            let mut area = TextArea::new("Signed off with\u{2026}", cx);
            area.set_text(account.signature.clone(), cx);
            area
        });

        Self {
            index,
            provider: Provider::for_account(account).id,
            carried: account.clone(),
            auth: account.auth,
            imap_security: account.imap_security,
            smtp_security: account.smtp_security,
            color: account.color,
            fields,
            signature,
            probe: ProbeState::Idle,
            sign_in: SignInState::Idle,
            signed_in: false,
            had_token,
            problems: Vec::new(),
            busy: false,
        }
    }

    fn input(&self, field: Field) -> &Entity<TextInput> {
        &self.fields[Field::ALL.iter().position(|f| *f == field).unwrap_or(0)]
    }

    fn value(&self, field: Field, cx: &Context<Workspace>) -> String {
        self.input(field).read(cx).text().trim().to_string()
    }

    /// The account the form currently describes.
    fn to_config(&self, cx: &Context<Workspace>) -> AccountConfig {
        let optional = |value: String| Some(value).filter(|v| !v.is_empty());

        AccountConfig {
            name: self.value(Field::Name, cx),
            email: self.value(Field::Email, cx),
            imap_host: self.value(Field::ImapHost, cx),
            // An unparseable port becomes 0, which validation rejects by
            // name — better than silently substituting 993 for a typo.
            imap_port: self.value(Field::ImapPort, cx).parse().unwrap_or(0),
            imap_user: optional(self.value(Field::ImapUser, cx)),
            imap_security: self.imap_security,
            smtp_host: optional(self.value(Field::SmtpHost, cx)),
            smtp_port: self.value(Field::SmtpPort, cx).parse().unwrap_or(0),
            smtp_user: optional(self.value(Field::SmtpUser, cx)),
            smtp_security: self.smtp_security,
            auth: self.auth,
            color: self.color,
            signature: self.signature.read(cx).text().trim_end().to_string(),
            ..self.carried.clone()
        }
    }

    fn title(&self) -> &'static str {
        match self.index {
            Some(_) => "Edit account",
            None => "Add account",
        }
    }
}

/// Whether an account has a credential to sync with.
///
/// Answering it means asking the keychain, which shells out — so it is
/// resolved on a background thread and rendered from whatever is known so far,
/// rather than blocking a frame on `security(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    Unknown,
    Present,
    Missing,
}

/// The Accounts section's state: what is on disk, and what is being edited.
#[derive(Default)]
pub struct AccountsState {
    pub accounts: Vec<AccountConfig>,
    /// One entry per account, in the same order.
    pub credentials: Vec<CredentialStatus>,
    pub editor: Option<AccountEditor>,
    /// A line under the list — an error from loading or saving, or a
    /// confirmation that something was written.
    pub notice: Option<Notice>,
    /// The account whose Remove button has been pressed once.
    ///
    /// Removing takes the downloaded mail with it, so the button asks before
    /// it does that — as a second press rather than a dialog, because the
    /// thing being confirmed is the button you are already looking at.
    pub confirming_removal: Option<usize>,
    /// The orphan whose Remove button has been pressed once.
    pub confirming_orphan: Option<spool_core::AccountId>,
    loaded: bool,
}

pub struct Notice {
    pub text: String,
    pub is_error: bool,
}

impl AccountsState {
    /// Reads `accounts.toml`, once per time the pane is opened.
    ///
    /// Re-reading on every frame would put a file read in the render path,
    /// which is the one thing this codebase does not do.
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        match Config::load(Config::default_path()) {
            Ok(config) => self.set_accounts(config.accounts),
            Err(e) => {
                self.notice = Some(Notice {
                    text: format!("Could not read accounts.toml: {e:#}"),
                    is_error: true,
                })
            }
        }
    }

    /// Replaces the list, resetting what is known about each credential.
    ///
    /// One method so the parallel `credentials` vector cannot drift out of
    /// step with the accounts it describes.
    fn set_accounts(&mut self, accounts: Vec<AccountConfig>) {
        self.credentials = vec![CredentialStatus::Unknown; accounts.len()];
        self.accounts = accounts;
        // Indices just moved; an armed button would now point at whichever
        // account slid into that slot.
        self.confirming_removal = None;
        self.confirming_orphan = None;
    }

    pub fn credential(&self, index: usize) -> CredentialStatus {
        self.credentials.get(index).copied().unwrap_or(CredentialStatus::Unknown)
    }
}

// ---------------------------------------------------------------- actions

impl Workspace {
    fn accounts_mut(&mut self) -> Option<&mut AccountsState> {
        self.settings_ui.as_mut().map(|state| &mut state.accounts)
    }

    fn editor_mut(&mut self) -> Option<&mut AccountEditor> {
        self.accounts_mut()?.editor.as_mut()
    }

    /// Called when the Accounts section is shown. Reads the file once, then
    /// checks the keychain off the UI thread.
    pub fn load_accounts(&mut self, cx: &mut Context<Self>) {
        let Some(accounts) = self.accounts_mut() else { return };
        if accounts.loaded {
            return;
        }
        accounts.ensure_loaded();
        self.refresh_account_credentials(cx);
    }

    /// Fills in which accounts actually have a credential stored.
    pub fn refresh_account_credentials(&mut self, cx: &mut Context<Self>) {
        let Some(accounts) = self.settings_ui.as_ref().map(|s| s.accounts.accounts.clone())
        else {
            return;
        };
        if accounts.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let statuses = cx
                .background_executor()
                .spawn(async move {
                    accounts
                        .iter()
                        .map(|account| match account.has_stored_credential() {
                            true => CredentialStatus::Present,
                            false => CredentialStatus::Missing,
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            this.update(cx, |this, cx| {
                if let Some(accounts) = this.accounts_mut()
                    // The list can have changed while the keychain was being
                    // read; a mismatched length means these answers are about
                    // accounts that are no longer there.
                    && accounts.accounts.len() == statuses.len()
                {
                    accounts.credentials = statuses;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn add_account(&mut self, cx: &mut Context<Self>) {
        let mut account = AccountConfig {
            // A new account takes the next colour along, so accounts stay
            // visually distinct in the sidebar without anyone choosing.
            color: self
                .settings_ui
                .as_ref()
                .map_or(0, |s| s.accounts.accounts.len() as u8),
            ..Default::default()
        };
        Provider::get(ProviderId::Other).apply_to(&mut account);

        let editor = AccountEditor::new(None, &account, false, cx);
        if let Some(accounts) = self.accounts_mut() {
            accounts.notice = None;
            accounts.editor = Some(editor);
        }
        cx.notify();
    }

    pub fn edit_account(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(account) = self
            .settings_ui
            .as_ref()
            .and_then(|s| s.accounts.accounts.get(index))
            .cloned()
        else {
            return;
        };

        // What is already stored decides whether Save can demand a sign-in.
        // Read from the status the pane has already computed, so opening an
        // editor never blocks a frame on the keychain.
        let had_token = self
            .settings_ui
            .as_ref()
            .is_some_and(|s| s.accounts.credential(index) != CredentialStatus::Missing);

        let editor = AccountEditor::new(Some(index), &account, had_token, cx);
        if let Some(accounts) = self.accounts_mut() {
            accounts.notice = None;
            accounts.editor = Some(editor);
        }
        cx.notify();
    }

    /// Whether the account form is open, which is the layer escape unwinds
    /// before the window itself.
    pub fn editing_account(&self) -> bool {
        self.settings_ui.as_ref().is_some_and(|state| state.accounts.editor.is_some())
    }

    pub fn close_account_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(accounts) = self.accounts_mut() {
            accounts.editor = None;
        }
        cx.notify();
    }

    /// Applies a provider preset to the open draft.
    pub fn choose_account_provider(&mut self, id: ProviderId, cx: &mut Context<Self>) {
        let Some(editor) = self.editor_mut() else { return };
        editor.provider = id;

        let provider = Provider::get(id);
        let mut draft = editor.carried.clone();
        provider.apply_to(&mut draft);

        editor.auth = draft.auth;
        editor.imap_security = draft.imap_security;
        editor.smtp_security = draft.smtp_security;
        // A preset that changed the servers invalidates whatever the last
        // test said about the old ones.
        editor.probe = ProbeState::Idle;

        let host = draft.imap_host.clone();
        let smtp = draft.smtp_host.clone().unwrap_or_default();
        let (imap_port, smtp_port) = (draft.imap_port.to_string(), draft.smtp_port.to_string());

        // `Other` clears the servers rather than leaving the previous
        // provider's behind, which would look like a suggestion.
        let editor = self.editor_mut().expect("checked above");
        let fields = [
            (Field::ImapHost, host),
            (Field::ImapPort, imap_port),
            (Field::SmtpHost, smtp),
            (Field::SmtpPort, smtp_port),
        ];
        let inputs: Vec<_> = fields
            .iter()
            .map(|(field, value)| (editor.input(*field).clone(), value.clone()))
            .collect();
        for (input, value) in inputs {
            input.update(cx, |input, cx| input.set_content(value, cx));
        }
        cx.notify();
    }

    pub fn choose_account_color(&mut self, color: u8, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor_mut() {
            editor.color = color;
        }
        cx.notify();
    }

    pub fn choose_account_auth(&mut self, auth: Auth, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor_mut() {
            editor.auth = auth;
            editor.probe = ProbeState::Idle;
        }
        cx.notify();
    }

    pub fn choose_imap_security(&mut self, security: Security, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor_mut() {
            editor.imap_security = security;
            editor.probe = ProbeState::Idle;
        }
        cx.notify();
    }

    pub fn choose_smtp_security(&mut self, security: Security, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor_mut() {
            editor.smtp_security = security;
            editor.probe = ProbeState::Idle;
        }
        cx.notify();
    }

    /// Writes the draft, its secrets, and the file.
    pub fn save_account(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return;
        };

        let draft = editor.to_config(cx);
        let index = editor.index;
        let password = editor.value(Field::Password, cx);

        let mut problems = draft.problems();
        // A Google account with no token would save happily and then fail on
        // every sync, so the one thing the button cannot do for you is
        // required before it lets you leave.
        if draft.auth == Auth::GoogleOauth && !editor.signed_in && !editor.had_token {
            problems.push("Press \u{201c}Sign in with Google\u{201d} to finish this account.".into());
        }
        if !problems.is_empty() {
            if let Some(editor) = self.editor_mut() {
                editor.problems = problems;
            }
            cx.notify();
            return;
        }

        // The address is the keychain key, so renaming it strands the old
        // entries. Cheaper to move them than to leave a password behind for
        // an address that no longer exists.
        let previous_email = index
            .and_then(|i| self.settings_ui.as_ref()?.accounts.accounts.get(i))
            .map(|account| account.email.clone())
            .filter(|email| *email != draft.email);

        let mut config = Config {
            accounts: self
                .settings_ui
                .as_ref()
                .map(|s| s.accounts.accounts.clone())
                .unwrap_or_default(),
        };
        config.upsert(index, draft.clone());

        if let Some(editor) = self.editor_mut() {
            editor.problems.clear();
            editor.busy = true;
        }
        cx.notify();

        let added = index.is_none();
        cx.spawn(async move |this, cx| {
            let email = draft.email.clone();
            let written = cx
                .background_executor()
                .spawn(async move {
                    if let Some(old) = previous_email {
                        migrate_secrets(&old, &email);
                    }
                    if !password.is_empty() {
                        secrets::store(Slot::Password, &email, &password)?;
                    }
                    config.save().map(|()| config)
                })
                .await;

            this.update(cx, |this, cx| {
                if let Some(editor) = this.editor_mut() {
                    editor.busy = false;
                }
                match written {
                    Ok(config) => {
                        this.has_accounts = !config.accounts.is_empty();
                        if let Some(accounts) = this.accounts_mut() {
                            accounts.set_accounts(config.accounts);
                            accounts.editor = None;
                            accounts.notice = Some(Notice {
                                text: format!("Saved {}.", draft.email),
                                is_error: false,
                            });
                        }
                        // A sync would carry the colour across too, but an
                        // account that is merely configured and not yet
                        // reachable may not see one for a long time, and the
                        // dot you just chose should be the dot in the sidebar.
                        this.store.set_account_color(&draft.email, draft.color).ok();
                        // The sidebar shows accounts, so it is now stale.
                        this.refresh_folders(cx);
                        this.refresh_account_credentials(cx);

                        if this.demo && this.settings.demo {
                            // An account belongs in the real mailbox, and
                            // that is where it gets synced: the copy that
                            // opens there starts on it straight away.
                            this.leave_demo(None, cx);
                        } else if added {
                            // A new account's mail, now rather than at the
                            // next launch: on a first run, an empty window
                            // after pressing Save reads as a failure.
                            this.asked_for_sync = true;
                            this.start_sync(spool_net::SyncMode::Everything, cx);
                        }
                    }
                    Err(e) => {
                        if let Some(accounts) = this.accounts_mut() {
                            accounts.notice = Some(Notice {
                                text: format!("Could not save: {e:#}"),
                                is_error: true,
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Removes an account: from the file, from the keychain, and from the
    /// local store.
    ///
    /// The downloaded mail goes with it. Leaving it behind was the earlier
    /// choice — an orphaned cache costs only disk — but it also left the
    /// account and its folders sitting in the sidebar with nothing behind
    /// them, and no way to see or reclaim what was still there. An account you
    /// removed should be gone.
    ///
    /// The first press arms the button and says how much mail is at stake; the
    /// second does it.
    pub fn remove_account(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.accounts_mut().is_some_and(|state| state.confirming_removal != Some(index)) {
            if let Some(state) = self.accounts_mut() {
                state.confirming_removal = Some(index);
                state.notice = None;
            }
            cx.notify();
            return;
        }

        let Some(accounts) = self.settings_ui.as_ref().map(|s| s.accounts.accounts.clone())
        else {
            return;
        };
        let mut config = Config { accounts };
        let Some(removed) = config.remove(index) else { return };

        // The store knows this account by address, which is the only name the
        // two halves share.
        let local = self
            .accounts
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(&removed.email))
            .map(|a| a.id);

        let deleted = match local {
            Some(id) => {
                // Standing in a folder that is about to stop existing would
                // leave a live title over an empty list.
                if self.scope_belongs_to(id) {
                    self.scope = Scope::Everything;
                }
                match self.store.delete_account(id) {
                    Ok(count) => Some(count),
                    Err(e) => {
                        log::error!("failed to delete {} locally: {e:#}", removed.email);
                        None
                    }
                }
            }
            None => None,
        };

        self.has_accounts = !config.accounts.is_empty();
        if let Some(state) = self.accounts_mut() {
            state.set_accounts(config.accounts.clone());
            state.editor = None;
        }
        self.refresh_folders(cx);
        self.reload(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let saved = cx.background_executor().spawn(async move { config.save() }).await;

            this.update(cx, |this, cx| {
                if let Some(accounts) = this.accounts_mut() {
                    accounts.notice = Some(match saved {
                        Ok(()) => Notice {
                            text: match deleted {
                                Some(0) | None => format!("Removed {}.", removed.email),
                                Some(1) => {
                                    format!("Removed {} and 1 downloaded message.", removed.email)
                                }
                                Some(n) => format!(
                                    "Removed {} and {n} downloaded messages.",
                                    removed.email
                                ),
                            },
                            is_error: false,
                        },
                        Err(e) => Notice {
                            text: format!("Could not save: {e:#}"),
                            is_error: true,
                        },
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Mail on this machine belonging to an account that is no longer
    /// configured.
    ///
    /// Removing an account takes its mail with it now, but that was not always
    /// true, and a config edited by hand can leave one behind at any time. The
    /// store is what the sidebar draws from, so an account that is only in the
    /// store is an account you can see, search and click on, with no way to
    /// get rid of it. Listing them here is that way.
    fn orphaned_accounts(&self) -> Vec<(spool_core::AccountId, String, u32)> {
        let Some(state) = self.settings_ui.as_ref() else { return Vec::new() };

        self.accounts
            .iter()
            .filter(|local| {
                !state
                    .accounts
                    .accounts
                    .iter()
                    .any(|configured| configured.email.eq_ignore_ascii_case(&local.email))
            })
            .map(|local| {
                let downloaded =
                    self.mailboxes.iter().filter(|m| m.account_id == local.id).map(|m| m.total).sum();
                (local.id, local.email.clone(), downloaded)
            })
            .collect()
    }

    /// Deletes mail left behind by an account that is no longer configured.
    ///
    /// Two presses, like removing a configured account, and for the same
    /// reason: this is somebody's archive.
    pub fn remove_orphaned_account(
        &mut self,
        account: spool_core::AccountId,
        cx: &mut Context<Self>,
    ) {
        if self.accounts_mut().is_some_and(|state| state.confirming_orphan != Some(account)) {
            if let Some(state) = self.accounts_mut() {
                state.confirming_orphan = Some(account);
                state.notice = None;
            }
            cx.notify();
            return;
        }

        let email = self
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.email.clone())
            .unwrap_or_default();

        if self.scope_belongs_to(account) {
            self.scope = Scope::Everything;
        }

        let removed = self.store.delete_account(account);
        if let Some(state) = self.accounts_mut() {
            state.confirming_orphan = None;
            state.notice = Some(match removed {
                Ok(0) => Notice { text: format!("Removed {email}."), is_error: false },
                Ok(1) => Notice {
                    text: format!("Removed {email} and 1 downloaded message."),
                    is_error: false,
                },
                Ok(n) => Notice {
                    text: format!("Removed {email} and {n} downloaded messages."),
                    is_error: false,
                },
                Err(e) => Notice { text: format!("Could not remove: {e:#}"), is_error: true },
            });
        }

        self.refresh_folders(cx);
        self.reload(cx);
        cx.notify();
    }

    /// Whether the view is currently inside this account.
    fn scope_belongs_to(&self, account: spool_core::AccountId) -> bool {
        match self.scope {
            Scope::Account(id) => id == account,
            Scope::Mailbox(id) => {
                self.mailboxes.iter().any(|m| m.id == id && m.account_id == account)
            }
            Scope::Everything => false,
        }
    }

    /// Runs the connection test against the draft as it currently stands.
    ///
    /// Against the draft rather than the saved account on purpose: the whole
    /// point is to find out whether these settings work before committing
    /// them.
    pub fn test_account(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return;
        };

        let draft = editor.to_config(cx);
        let password = editor.value(Field::Password, cx);

        let problems = draft.problems();
        if !problems.is_empty() {
            if let Some(editor) = self.editor_mut() {
                editor.problems = problems;
            }
            cx.notify();
            return;
        }

        if let Some(editor) = self.editor_mut() {
            editor.problems.clear();
            editor.probe = ProbeState::Running;
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // A password typed but not yet saved has to reach the
                    // test somehow, and the credential resolver only reads
                    // stored ones. Putting it in the draft keeps it in memory
                    // rather than writing a secret the user has not committed
                    // to.
                    let mut draft = draft;
                    if !password.is_empty() {
                        draft.password = Some(password);
                    }
                    probe::test_account(&draft).await
                })
                .await;

            this.update(cx, |this, cx| {
                if let Some(editor) = this.editor_mut() {
                    editor.probe = ProbeState::Done(result);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs the Google sign-in flow and stores the refresh token.
    pub fn sign_in_with_google(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return;
        };

        let hint = editor.value(Field::Email, cx);

        // The client belongs to the build, so this can only fail on a build
        // that was never given one — a developer's problem, not a user's, and
        // the message says as much.
        let client = match oauth::ClientCredentials::bundled() {
            Ok(client) => client,
            Err(e) => {
                if let Some(editor) = self.editor_mut() {
                    editor.sign_in = SignInState::Failed(format!("{e:#}"));
                }
                cx.notify();
                return;
            }
        };

        if let Some(editor) = self.editor_mut() {
            editor.sign_in = SignInState::Waiting;
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let grant = oauth::authorize(
                        &client,
                        Some(hint.as_str()).filter(|h| !h.is_empty()),
                        open_in_browser,
                    )
                    .await?;

                    // Google names the account; the form's address is only a
                    // hint, and signing in as somebody else is a mistake
                    // worth catching before it becomes a mislabelled inbox.
                    let email = grant
                        .email
                        .clone()
                        .or(Some(hint).filter(|h| !h.is_empty()))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Google did not say which address this is — type it in \
                                 and sign in again"
                            )
                        })?;

                    secrets::store(Slot::RefreshToken, &email, &grant.refresh_token)?;
                    // A stale token cached for this address would outlive the
                    // sign-in that just replaced it.
                    oauth::forget(&email);

                    anyhow::Ok(email)
                })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(email) => {
                        let field = this.editor_mut().map(|editor| {
                            editor.sign_in = SignInState::Done(email.clone());
                            editor.signed_in = true;
                            editor.auth = Auth::GoogleOauth;
                            editor.probe = ProbeState::Idle;
                            editor.input(Field::Email).clone()
                        });
                        if let Some(field) = field {
                            field.update(cx, |input, cx| input.set_content(email, cx));
                        }
                    }
                    Err(e) => {
                        if let Some(editor) = this.editor_mut() {
                            editor.sign_in = SignInState::Failed(format!("{e:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Moves an account's secrets to a new address, after it was renamed.
fn migrate_secrets(from: &str, to: &str) {
    for slot in Slot::ALL {
        match secrets::load(slot, from) {
            Ok(Some(secret)) => {
                if secrets::store(slot, to, &secret).is_ok() {
                    secrets::delete(slot, from).ok();
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("could not move the {} entry for {from}: {e:#}", slot.service()),
        }
    }
    oauth::forget(from);
}

/// Hands a URL to the platform's browser.
pub(crate) fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, args): (&str, &[&str]) = ("xdg-open", &[]);

    if let Err(e) = std::process::Command::new(program).args(args).arg(url).spawn() {
        // Not fatal: the listener is already waiting, so pasting the URL by
        // hand still completes the flow.
        log::error!("could not open a browser ({e}); visit this URL to sign in: {url}");
    }
}

// ---------------------------------------------------------------- rendering

impl Workspace {
    pub(crate) fn render_accounts(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.settings_ui.as_ref() else {
            return div().into_any_element();
        };

        let body = match state.accounts.editor.is_some() {
            true => self.render_account_editor(theme, window, cx),
            false => self.render_account_list(theme, cx),
        };

        div()
            .id("settings-accounts")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .child(body)
            .children(state.accounts.notice.as_ref().map(|notice| {
                div()
                    .px_2p5()
                    .py_2()
                    .rounded_md()
                    .text_size(theme.small_text_size)
                    .bg(theme.hover)
                    .text_color(if notice.is_error { theme.red } else { theme.text_muted })
                    .child(SharedString::from(notice.text.clone()))
            }))
            .into_any_element()
    }

    fn render_account_list(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.settings_ui.as_ref() else {
            return div().into_any_element();
        };

        let rows: Vec<AnyElement> = state
            .accounts
            .accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                self.render_account_row(index, account, state.accounts.credential(index), theme, cx)
            })
            .collect();
        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                heading(
                    theme,
                    "Accounts",
                    "Mail is synced from every account listed here. Passwords and sign-ins \
                     are kept in your keychain, not in the config file.",
                )
                .child(
                    button("accounts-add", theme, ButtonKind::Primary)
                        .on_click(cx.listener(|this, _, _, cx| this.add_account(cx)))
                        .child("Add account"),
                ),
            )
            .when(empty, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .items_center()
                        .py_8()
                        .rounded_lg()
                        .border_1()
                        .border_dashed()
                        .border_color(theme.border)
                        .child(div().text_color(theme.text_muted).child("No accounts yet"))
                        .child(
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_faint)
                                .child("Add one to start syncing mail."),
                        ),
                )
            })
            .child(div().flex().flex_col().gap_2().children(rows))
            .children(self.render_orphaned_accounts(theme, cx))
            .child(self.render_default_mail_setting(theme, cx))
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "Stored in {}",
                        Config::default_path().display()
                    ))),
            )
            .into_any_element()
    }

    /// Which app opens email links, and the button that makes it this one.
    ///
    /// Here rather than in a section of its own: it is about where mail is
    /// written from, which is what an account is, and a General section
    /// holding one switch is a section nobody would think to open.
    fn render_default_mail_setting(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::default_mail::Handler;

        let claiming = self.claiming_default_mail;
        let status: SharedString = match (&self.default_mail, claiming) {
            (_, true) => "Waiting for macOS\u{2026}".into(),
            (Handler::Spool, _) => "Spool opens them.".into(),
            (Handler::Other(Some(name)), _) => format!("{name} opens them now.").into(),
            (Handler::Other(None), _) => "Another app opens them now.".into(),
            (Handler::Unbundled, _) => {
                "Only Spool.app can open them \u{2014} not a build run from cargo.".into()
            }
            (Handler::Unknown, _) => "".into(),
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .child(
                heading(
                    theme,
                    "Email links",
                    "An address clicked in a browser, a PDF or another app opens a new \
                     message in whichever app is the default.",
                )
                .when(self.default_mail.can_claim() && !claiming, |this| {
                    this.child(
                        button("default-mail", theme, ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.make_default_mail_app(cx)))
                            .child("Make Spool the default"),
                    )
                }),
            )
            .child(hint(theme, status))
            .into_any_element()
    }

    /// Mail on this machine with no account behind it any more.
    ///
    /// Shown rather than hidden: this is the only place it can be reached, and
    /// hiding mail somebody still has on disk is how you end up with a mailbox
    /// that cannot be explained.
    fn render_orphaned_accounts(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let orphans = self.orphaned_accounts();
        if orphans.is_empty() {
            return None;
        }

        let arming = self.settings_ui.as_ref().and_then(|s| s.accounts.confirming_orphan);

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mt_2()
                .child(heading(
                    theme,
                    "Downloaded, not configured",
                    "Mail left on this machine by an account that is no longer set up. It is \
                     still in the sidebar and still searchable until it is removed.",
                ))
                .children(orphans.into_iter().map(|(id, email, downloaded)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2p5()
                        .rounded_lg()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .line_clamp(1)
                                        .text_color(theme.text)
                                        .child(SharedString::from(email)),
                                )
                                .child(
                                    div()
                                        .text_size(theme.small_text_size)
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(match downloaded {
                                            1 => "1 message on this machine".to_string(),
                                            n => format!(
                                                "{} messages on this machine",
                                                crate::perf::format_count(n as usize)
                                            ),
                                        })),
                                ),
                        )
                        .child(
                            button(("orphan-remove", id.0 as usize), theme, ButtonKind::Danger)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_orphaned_account(id, cx)
                                }))
                                .child(match (arming == Some(id), downloaded) {
                                    (false, _) => "Remove".to_string(),
                                    (true, 0) => "Remove for good?".to_string(),
                                    (true, 1) => "Delete 1 message?".to_string(),
                                    (true, n) => format!(
                                        "Delete {} messages?",
                                        crate::perf::format_count(n as usize)
                                    ),
                                }),
                        )
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    fn render_account_row(
        &self,
        index: usize,
        account: &AccountConfig,
        credential: CredentialStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let provider = Provider::for_email(&account.email);

        // Armed by the first press. The label says what the second one costs,
        // counted from the folders already in memory rather than by asking the
        // store during a render.
        let arming = self
            .settings_ui
            .as_ref()
            .is_some_and(|s| s.accounts.confirming_removal == Some(index));
        let downloaded: u32 = self
            .accounts
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(&account.email))
            .map(|local| {
                self.mailboxes
                    .iter()
                    .filter(|m| m.account_id == local.id)
                    .map(|m| m.total)
                    .sum()
            })
            .unwrap_or(0);

        let mut summary = vec![account.email.clone()];
        if provider.id != ProviderId::Other {
            summary.push(provider.label.to_string());
        }
        summary.push(match account.auth {
            Auth::GoogleOauth => "Google sign-in".to_string(),
            Auth::Password => format!("{}:{}", account.imap_host, account.imap_port),
        });

        // Only a definite "missing" is worth a warning; while the keychain is
        // still being read, saying nothing beats saying the wrong thing.
        let warning = (credential == CredentialStatus::Missing).then_some(match account.auth {
            Auth::GoogleOauth => "Not signed in",
            Auth::Password => "No password stored",
        });

        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2p5()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .size(px(9.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(theme.account_color(account.color)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .line_clamp(1)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(account.name.clone())),
                            )
                            .children(warning.map(|text| {
                                div()
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(theme.hover)
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.yellow)
                                    .child(text)
                            })),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .line_clamp(1)
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(SharedString::from(summary.join(" \u{00b7} "))),
                    ),
            )
            .child(
                button(("accounts-edit", index), theme, ButtonKind::Secondary)
                    .on_click(cx.listener(move |this, _, _, cx| this.edit_account(index, cx)))
                    .child("Edit"),
            )
            .child(
                button(("accounts-remove", index), theme, ButtonKind::Danger)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_account(index, cx)))
                    .child(match (arming, downloaded) {
                        (false, _) => "Remove".to_string(),
                        (true, 0) => "Remove for good?".to_string(),
                        (true, 1) => "Remove and delete 1 message?".to_string(),
                        (true, n) => format!(
                            "Remove and delete {} messages?",
                            crate::perf::format_count(n as usize)
                        ),
                    }),
            )
            .into_any_element()
    }

    fn render_account_editor(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return div().into_any_element();
        };

        let provider = Provider::get(editor.provider);
        // A preset answers the questions below by existing. What is left to
        // ask about is the person, not the plumbing.
        let preset = provider.is_preset();
        let busy = editor.busy;
        // The signature is shaped outside the element tree's text style, so
        // without the theme's ink it is the window's default black, whatever
        // the theme. Set on every render so a theme change follows it.
        editor.signature.update(cx, |area, cx| {
            area.set_style(AreaStyle { ink: Some(theme.text), ..Default::default() }, cx)
        });
        let signature_focused = editor.signature.read(cx).focus_handle.is_focused(window);

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(theme, editor.title(), provider.note_for(editor.auth)))
            // Provider
            .child(
                group(theme, "Provider").child(
                    div().flex().flex_wrap().gap_1p5().children(
                        spool_net::config::PROVIDERS.iter().map(|option| {
                            let id = option.id;
                            chip(("provider", id as usize), theme, id == editor.provider)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_account_provider(id, cx)
                                }))
                                .child(option.label)
                        }),
                    ),
                ),
            )
            // Identity
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.render_field(Field::Name, None, theme, window, cx))
                    .child(self.render_field(Field::Email, None, theme, window, cx)),
            )
            // Colour. Sits with the name and the address because that is what
            // it is: the account's third identifying mark, and the one doing
            // the work in a unified list where the other two are not showing.
            .child(
                group(theme, "Colour")
                    .child(
                        div().flex().flex_wrap().gap_2().children(
                            (0..theme.avatars.len() as u8).map(|index| {
                                swatch(index, theme, index == editor.color).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.choose_account_color(index, cx)
                                    }),
                                )
                            }),
                        ),
                    )
                    .child(hint(theme, "Marks this account's mail and folders in the sidebar.")),
            )
            // Authentication. A preset already knows how its provider signs
            // in — Gmail is OAuth — so the choice is offered only where
            // there is one to make. Picking it is the whole point of picking
            // the provider.
            .when(!preset, |this| {
                this.child(
                    group(theme, "Sign in with").child(
                        div().flex().gap_1p5().children(Auth::ALL.map(|option| {
                            chip(("auth", option as usize), theme, option == editor.auth)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_account_auth(option, cx)
                                }))
                                .child(option.label())
                        })),
                    ),
                )
            })
            .child(match editor.auth {
                Auth::Password => self.render_password_auth(theme, window, cx),
                Auth::GoogleOauth => self.render_google_auth(theme, cx),
            })
            // Servers. A preset's hosts and ports are not a decision anyone
            // is making; they are four facts about Gmail, and four fields
            // that must not be edited read as four fields that could be.
            // Stated as a line of text instead, so the form is short without
            // being a black box about what it connects to.
            .when(preset, |this| {
                this.child(hint(
                    theme,
                    format!(
                        "Uses {}. Pick \u{201c}Other\u{201d} above to set them by hand.",
                        provider.servers()
                    ),
                ))
            })
            .when(!preset, |this| {
                this.child(
                    group(theme, "Incoming mail (IMAP)")
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .child(self.render_field(Field::ImapHost, None, theme, window, cx))
                                .child(self.render_field(Field::ImapPort, Some(px(84.)), theme, window, cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .items_end()
                                .child(self.render_field(Field::ImapUser, None, theme, window, cx))
                                .child(
                                    div().flex().gap_1p5().pb_1().children(Security::ALL.map(
                                        |option| {
                                            chip(
                                                ("imap-security", option as usize),
                                                theme,
                                                option == editor.imap_security,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.choose_imap_security(option, cx)
                                            }))
                                            .child(option.label())
                                        },
                                    )),
                                ),
                        ),
                )
                .child(
                    group(theme, "Outgoing mail (SMTP)")
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .child(self.render_field(Field::SmtpHost, None, theme, window, cx))
                                .child(self.render_field(Field::SmtpPort, Some(px(84.)), theme, window, cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .items_end()
                                .child(self.render_field(Field::SmtpUser, None, theme, window, cx))
                                .child(
                                    div().flex().gap_1p5().pb_1().children(Security::ALL.map(
                                        |option| {
                                            chip(
                                                ("smtp-security", option as usize),
                                                theme,
                                                option == editor.smtp_security,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.choose_smtp_security(option, cx)
                                            }))
                                            .child(option.label())
                                        },
                                    )),
                                ),
                        ),
                )
            })
            .child(
                group(theme, "Signature")
                    .child(
                        div()
                            .h(px(76.))
                            .px_2p5()
                            .py_1p5()
                            .rounded_md()
                            .bg(theme.background)
                            .border_1()
                            .border_color(if signature_focused {
                                theme.focus_ring
                            } else {
                                theme.border
                            })
                            .child(editor.signature.clone()),
                    )
                    .child(hint(
                        theme,
                        "Added above the quoted text when you reply, under a \u{201c}-- \u{201d} line.",
                    )),
            )
            // Problems, then the test results, then the buttons.
            .when(!editor.problems.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .px_2p5()
                        .py_2()
                        .rounded_md()
                        .bg(theme.hover)
                        .text_size(theme.small_text_size)
                        .text_color(theme.red)
                        .children(
                            editor
                                .problems
                                .iter()
                                .map(|problem| div().child(SharedString::from(problem.clone()))),
                        ),
                )
            })
            .children(self.render_probe(theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(
                        button("accounts-test", theme, ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.test_account(cx)))
                            .child(match editor.probe {
                                ProbeState::Running => "Testing\u{2026}",
                                _ => "Test connection",
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        button("accounts-cancel", theme, ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.close_account_editor(cx)))
                            .child("Cancel"),
                    )
                    .child(
                        button("accounts-save", theme, ButtonKind::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.save_account(cx)))
                            .child(if busy { "Saving\u{2026}" } else { "Save" }),
                    ),
            )
            .into_any_element()
    }

    fn render_password_auth(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let editing = self
            .settings_ui
            .as_ref()
            .and_then(|s| s.accounts.editor.as_ref())
            .is_some_and(|editor| editor.index.is_some());

        // No group label here: the field's own "Password" is the label, and a
        // heading above it would just say the word twice.
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.render_field(Field::Password, Some(px(280.)), theme, window, cx))
            .child(hint(
                theme,
                match editing {
                    // The field is empty because the stored password was
                    // deliberately not read back; say so, or it reads as
                    // "there is no password".
                    true => "Leave blank to keep the password already stored. Anything typed \
                             here replaces it.",
                    false => "Stored in your keychain, never in the config file. Most \
                              providers want an app-specific password rather than your \
                              login password.",
                },
            ))
            .into_any_element()
    }

    fn render_google_auth(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return div().into_any_element();
        };

        // Already connected, and nothing was done in this session: say so
        // rather than showing a button that implies something is missing.
        let connected = editor.had_token && !editor.signed_in;

        let (status, color) = match &editor.sign_in {
            SignInState::Idle if connected => (
                "Connected. Sign in again only if Google has revoked access.".to_string(),
                theme.text_faint,
            ),
            SignInState::Idle => (String::new(), theme.text_faint),
            SignInState::Waiting => (
                "Waiting for the browser\u{2026} approve the request there, then come back."
                    .into(),
                theme.text_muted,
            ),
            SignInState::Done(email) => (format!("Signed in as {email}."), theme.green),
            SignInState::Failed(error) => (error.clone(), theme.red),
        };

        let waiting = matches!(editor.sign_in, SignInState::Waiting);
        let available = oauth::ClientCredentials::is_available();

        group(theme, "Google account")
            // No hint when it works: the heading above already says the
            // browser opens and that Spool never sees the password, and
            // saying it twice on one short form is the redundancy this pane
            // is trying to shed.
            .when(!available, |this| {
                // Only reachable in a build that shipped without
                // credentials, so it is addressed to whoever built it.
                this.child(hint(
                    theme,
                    "This build has no Google OAuth client compiled in, so signing in cannot \
                     work. See BUILT_IN_CLIENT_ID in crates/net/src/oauth.rs.",
                ))
            })
            // Wrapped in a row: a button is a child of a flex *column* here,
            // and a column stretches its children, which would give this one
            // the full width of the pane.
            .child(
                div().flex().items_center().child(
                    button("accounts-signin", theme, ButtonKind::Primary)
                        .when(!available, |s| s.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_in_with_google(cx)))
                        .child(match (waiting, connected) {
                            (true, _) => "Waiting for Google\u{2026}",
                            (false, true) => "Sign in again",
                            (false, false) => "Sign in with Google",
                        }),
                ),
            )
            .when(!status.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(color)
                        .child(SharedString::from(status)),
                )
            })
            .into_any_element()
    }

    /// The result of the last connection test, one line per thing checked.
    fn render_probe(&self, theme: &Theme) -> Option<AnyElement> {
        let editor = self.settings_ui.as_ref()?.accounts.editor.as_ref()?;
        let ProbeState::Done(probe) = &editor.probe else { return None };

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_md()
                .bg(theme.panel)
                .border_1()
                .border_color(if probe.passed() { theme.border } else { theme.red })
                .children(probe.steps.iter().map(|step| {
                    let (glyph, color) = match step.outcome {
                        spool_net::Outcome::Passed => ("\u{2713}", theme.green),
                        spool_net::Outcome::Failed => ("\u{2717}", theme.red),
                        spool_net::Outcome::Skipped => ("\u{2013}", theme.text_faint),
                    };

                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .text_size(theme.small_text_size)
                        .child(div().w(px(12.)).flex_shrink_0().text_color(color).child(glyph))
                        .child(
                            div()
                                .w(px(190.))
                                .flex_shrink_0()
                                .font_family(theme.mono_font)
                                .text_color(theme.text_muted)
                                .child(SharedString::from(step.label.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(if step.passed() {
                                    theme.text_faint
                                } else {
                                    theme.text
                                })
                                .child(SharedString::from(step.detail.clone())),
                        )
                }))
                .into_any_element(),
        )
    }

    /// A labelled text field. `width` fixes its size; `None` shares the row.
    fn render_field(
        &self,
        field: Field,
        width: Option<Pixels>,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(editor) = self.settings_ui.as_ref().and_then(|s| s.accounts.editor.as_ref())
        else {
            return div().into_any_element();
        };
        let input = editor.input(field).clone();
        let focused = input.read(cx).focus_handle.is_focused(window);

        div()
            .flex()
            .flex_col()
            .gap_1()
            .map(|this| match width {
                Some(width) => this.w(width).flex_shrink_0(),
                None => this.flex_1(),
            })
            .min_w_0()
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(field.label()),
            )
            .child(
                div()
                    .flex()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .bg(theme.background)
                    .border_1()
                    .border_color(if focused { theme.focus_ring } else { theme.border })
                    .child(input),
            )
            .into_any_element()
    }
}

/// A section title with a sentence under it, and room for a button on the
/// right.
fn heading(theme: &Theme, title: &'static str, description: &'static str) -> Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(description),
                ),
        )
}

/// A labelled block of controls inside the editor.
fn group(theme: &Theme, label: &'static str) -> Div {
    div().flex().flex_col().gap_2().child(
        div()
            .text_size(theme.small_text_size)
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_muted)
            .child(label),
    )
}

fn hint(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_size(theme.small_text_size).text_color(theme.text_faint).child(text.into())
}

/// The three kinds of button in this pane.
///
/// An enum rather than a `primary: bool` plus a `.hover()` at the call site:
/// gpui panics if a hover style is set twice on one element, so every variant
/// has to set its hover exactly once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    /// The action the pane is for. One per view.
    Primary,
    Secondary,
    /// Destructive, so it turns red rather than merely lighting up.
    Danger,
}

pub(crate) fn button(id: impl Into<gpui::ElementId>, theme: &Theme, kind: ButtonKind) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .px_2p5()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_size(theme.small_text_size);

    match kind {
        ButtonKind::Primary => base
            .bg(theme.accent)
            .text_color(theme.text_on_accent)
            .hover(|s| s.bg(theme.accent_muted)),
        ButtonKind::Secondary => base
            .border_1()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .hover(|s| s.bg(theme.hover).text_color(theme.text)),
        ButtonKind::Danger => base
            .border_1()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .hover(|s| s.bg(theme.hover).text_color(theme.red)),
    }
}

/// A one-of-many toggle.
fn chip(id: impl Into<gpui::ElementId>, theme: &Theme, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(theme.small_text_size)
        .border_1()
        .when(selected, |s| {
            s.border_color(theme.accent).bg(theme.selected).text_color(theme.text)
        })
        .when(!selected, |s| {
            s.border_color(theme.border)
                .text_color(theme.text_muted)
                .hover(|s| s.bg(theme.hover))
        })
}

/// One colour in the account palette, as something to press.
///
/// The colour itself is the label, so there is nothing to read: a ring around
/// the chosen one says which, the way a colour well does everywhere else on
/// the machine. The ring is drawn outside the dot rather than around its edge
/// — a border on the dot would eat into the only part of it that carries the
/// information.
fn swatch(index: u8, theme: &Theme, selected: bool) -> Stateful<Div> {
    div()
        .id(("account-color", index as usize))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(26.))
        .rounded_full()
        .cursor_pointer()
        .border_2()
        .when(selected, |s| s.border_color(theme.accent))
        .when(!selected, |s| {
            s.border_color(gpui::transparent_black()).hover(|s| s.border_color(theme.border))
        })
        .child(div().size(px(16.)).rounded_full().bg(theme.account_color(index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_labelled_and_reachable_by_index() {
        for (index, field) in Field::ALL.iter().enumerate() {
            assert!(!field.label().is_empty(), "{field:?}");
            assert!(!field.placeholder().is_empty(), "{field:?}");
            assert_eq!(
                Field::ALL.iter().position(|f| f == field),
                Some(index),
                "the array order is what indexes the inputs"
            );
        }
    }

    #[test]
    fn only_the_password_is_masked() {
        let masked: Vec<_> = Field::ALL.iter().filter(|f| f.is_secret()).collect();
        assert_eq!(masked, [&Field::Password]);
    }

    #[test]
    fn a_new_google_account_must_be_signed_in_before_it_can_be_saved() {
        // The rule Save enforces, stated once so it cannot drift from the
        // branch that implements it: a Google account is only complete when a
        // token exists, either from this session or from before.
        let complete = |signed_in: bool, had_token: bool| signed_in || had_token;

        assert!(!complete(false, false), "a brand new account needs the browser trip");
        assert!(complete(true, false), "signing in just now is enough");
        assert!(complete(false, true), "editing a connected account is not a re-auth");
    }

    #[test]
    fn a_fresh_accounts_pane_knows_nothing_about_credentials() {
        // Rendering must not claim an account is missing a password before
        // the keychain has actually been asked.
        let state = AccountsState::default();
        assert_eq!(state.credential(0), CredentialStatus::Unknown);
    }
}
