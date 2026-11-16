//! What the window does with extensions.
//!
//! [`crate::extensions`] owns the engine and knows how to call a component.
//! This is the other side: when the window calls it, and what it does with the
//! answer. One rule shapes all of it — **a component is never entered on the
//! UI thread.** Every method here spawns, waits, and comes back to apply plain
//! data.
//!
//! Effects are what makes that split affordable. An extension changes nothing;
//! it returns a list of changes it would like, and the window applies them
//! itself once the call is over. So the thing that crosses back from a
//! background thread is a `Vec<Effect>` — values that can be logged, counted,
//! refused, and, because they go through the same helpers the keyboard does,
//! undone with ⌘Z like anything else.

use std::collections::{HashMap, HashSet};

use gpui::{Context, prelude::*};
use spool_core::{Envelope, Flags, MailboxId, MailboxRole, MessageId};
use spool_ext::{api::Effect, manifest::span_seconds, runtime::CommandContext};
use spool_store::{Query, Scope};

use crate::{
    actions::{Command, RunExtensionCommand, commands},
    extensions::{Host, Outcome},
    workspace::Workspace,
};

/// How many messages of a backlog go to an extension in one call.
///
/// One call is what `on-messages` is given for a sync's worth of new mail,
/// and an extension that talks to a network may only get through so many of
/// them inside its deadline. What a call leaves undone, the next one is
/// handed again — an extension skips what it has already seen — so this is
/// about how often progress is shown more than how much gets done.
const SCAN_BATCH: usize = 24;

/// Asks an extension about every loaded message and sorts the list into its
/// filters: the count in each, and — with one of them chosen — only the
/// messages in it. On a background thread, inside a reload.
pub(crate) fn sort_into_filters(
    host: &Host,
    extension: &str,
    chosen: Option<(String, String)>,
    envelopes: &mut Vec<Envelope>,
) -> Vec<(String, usize)> {
    let Some((counts, filters)) = filters_and_counts(host, extension, envelopes) else {
        return Vec::new();
    };
    if let Some((_, wanted)) = chosen.filter(|(e, _)| e == extension) {
        let mut filters = filters.into_iter();
        envelopes.retain(|_| filters.next().is_some_and(|of_one| of_one.contains(&wanted)));
    }
    counts
}

/// Only the counts above the list: [`sort_into_filters`] with nothing
/// chosen. Separate because nothing chosen is the one case where the list
/// does not have to wait for it — the rows are the same either way.
pub(crate) fn count_filters(host: &Host, extension: &str, envelopes: &[Envelope]) -> Vec<(String, usize)> {
    filters_and_counts(host, extension, envelopes).map(|(counts, _)| counts).unwrap_or_default()
}

/// Each filter of an extension, and how many messages are in it.
type FilterCounts = Vec<(String, usize)>;

/// Which of the extension's filters each message is in, and how many are in
/// each.
fn filters_and_counts(
    host: &Host,
    extension: &str,
    envelopes: &[Envelope],
) -> Option<(FilterCounts, Vec<Vec<String>>)> {
    let installed = host.get(extension)?;
    // A batch at a time rather than the whole mailbox in one call: one call
    // holds the extension for all of it, and the pass for the rows on screen
    // would wait behind a hundred thousand rows nobody is looking at.
    for batch in envelopes.chunks(DECORATE_BATCH) {
        host.decorate(batch);
    }

    let ids: Vec<MessageId> = envelopes.iter().map(|e| e.id).collect();
    let filters = host.filters_of(extension, &ids);

    let mut counts: Vec<(String, usize)> =
        installed.manifest.filters.iter().map(|f| (f.id.to_string(), 0)).collect();
    let declared = counts.len();
    for of_one in &filters {
        for filter in of_one {
            match counts.iter_mut().find(|(id, _)| id == filter) {
                Some((_, count)) => *count += 1,
                // A badge's own filter, met for the first time.
                None => counts.push((filter.clone(), 1)),
            }
        }
    }
    // The declared ones in the manifest's order, then the rest by size.
    counts[declared..].sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Some((counts, filters))
}

/// How many messages a single decoration pass will ask about.
///
/// `decorate` has the shortest deadline of any call — a twentieth of a second,
/// because a list row is waiting on the cache it fills — so a pass is bounded
/// by this rather than by however much mail is loaded. What is left over is
/// picked up by the next pass.
const DECORATE_BATCH: usize = 200;

impl Workspace {
    /// Everything the palette can run: this app's commands, then whatever is
    /// contributed.
    ///
    /// Available without loading a single component. The registry reads the
    /// manifests, so an extension is listed, searchable and runnable while
    /// still costing nothing until somebody actually picks one of its
    /// commands — which is when it is compiled.
    pub fn palette_commands(&self) -> Vec<Command> {
        let mut all = commands();
        // Clients' commands exist only once it has been added.
        if !self.settings.clients {
            all.retain(|command| {
                let action = command.action.as_any();
                !action.is::<crate::actions::OpenClients>() && !action.is::<crate::actions::OpenClientBoard>()
            });
        }

        let Some(host) = &self.extensions else { return all };

        for contributed in host.commands() {
            all.push(Command {
                // The extension's name beside the title, because two
                // extensions will eventually both contribute a "Refresh" and
                // the row has to say which is which.
                name: format!("{} \u{2014} {}", contributed.title, contributed.extension).into(),
                keystroke: contributed.key.clone().unwrap_or_default().into(),
                action: Box::new(RunExtensionCommand { id: contributed.qualified }),
            });
        }

        all
    }

    /// Starts the extensions that have to be live before anybody asks for one.
    ///
    /// On a background thread, after the window is up. Compiling components in
    /// front of the first frame would mean every launch paid for extensions,
    /// including the launches where none of them ever do anything.
    pub fn start_extensions(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        if !host.any() {
            return;
        }

        self.bind_extension_keys(cx);
        self.redraw_extension_panels(cx);
        self.resume_scans(cx);

        cx.background_executor()
            .spawn(async move {
                host.start_eager();
            })
            .detach();
    }

    /// Gives extensions the keystrokes they asked for, where they are free.
    ///
    /// Advisory, as the manifest says. A keystroke this app binds stays bound
    /// to what it was bound to — an extension that could take `cmd-z` away
    /// from undo by being installed is an extension that breaks the app by
    /// existing — and the command is still in the palette either way.
    fn bind_extension_keys(&self, cx: &mut Context<Self>) {
        let Some(host) = &self.extensions else { return };

        let claimed = crate::actions::claimed_keystrokes();
        let mut bindings = Vec::new();

        for contributed in host.commands() {
            let Some(key) = contributed.key.as_deref().map(str::trim).filter(|k| !k.is_empty())
            else {
                continue;
            };

            if claimed.contains(key) {
                log::info!(
                    "`{}` asked for `{key}`, which Spool binds already — it is in the palette",
                    contributed.qualified,
                );
                continue;
            }

            bindings.push(gpui::KeyBinding::new(
                key,
                RunExtensionCommand { id: contributed.qualified.clone() },
                // The mailbox window only. A keystroke that reached the
                // composer would fire an extension command over a half-written
                // message.
                Some("Spool"),
            ));
        }

        if !bindings.is_empty() {
            log::info!("bound {} extension keystroke(s)", bindings.len());
            cx.bind_keys(bindings);
        }
    }

    /// Builds one text field per declared setting, filled from `config.toml`.
    ///
    /// Done when the section is opened rather than when the window is: most
    /// people never open it, and a field is an entity with a focus handle and
    /// a subscription rather than a string.
    ///
    /// Secrets *are* loaded back in, unlike the account password field. The
    /// difference is where they live: a mail password is in the keychain and a
    /// form pre-filled with a working one can destroy it by accident, whereas
    /// this value is the plain text of a file sitting next to the manifest.
    /// Blanking the field here and saving is how you remove it, and that has
    /// to be possible.
    pub(crate) fn load_extension_settings(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };

        // Built once. Leaving this section and coming back must not throw away
        // a key somebody had half-typed, and the fields already agree with the
        // file everywhere else — saving is what writes them, and a save is
        // what re-reads.
        if self
            .settings_ui
            .as_ref()
            .is_some_and(|s| !s.extensions.fields.is_empty() || !s.extensions.lists.is_empty())
        {
            return;
        }

        let mut fields = std::collections::BTreeMap::new();
        let mut lists = std::collections::BTreeMap::new();
        for installed in host.installed() {
            for decl in installed.manifest.settings() {
                if decl.is_list() {
                    let rows: Vec<std::collections::BTreeMap<String, String>> = installed
                        .config
                        .get(decl.key.as_str())
                        .and_then(|json| serde_json::from_str(json).ok())
                        .unwrap_or_default();
                    let rows = rows.iter().map(|row| list_row(decl, row, cx)).collect();
                    lists.insert((installed.id().to_string(), decl.key.to_string()), rows);
                    continue;
                }
                let value = installed.config.get(decl.key.as_str()).unwrap_or_default().to_string();
                let placeholder = match decl.default.as_deref() {
                    Some(default) => format!("{default}"),
                    None => decl.title.clone(),
                };

                let input = cx.new(|cx| {
                    let mut input = match decl.secret {
                        true => crate::text_input::TextInput::masked(placeholder, cx),
                        false => crate::text_input::TextInput::new(placeholder, cx),
                    };
                    if !value.is_empty() {
                        input.set_content(value, cx);
                    }
                    input
                });

                fields.insert((installed.id().to_string(), decl.key.to_string()), input);
            }
        }

        if let Some(state) = self.settings_ui.as_mut() {
            state.extensions.fields = fields;
            state.extensions.lists = lists;
            state.extensions.edited.clear();
        }
        cx.notify();
    }

    /// Writes one extension's `config.toml` from its fields, and applies it.
    ///
    /// The whole file each time, from the declared settings, so clearing a
    /// field removes the key rather than setting it to an empty string that
    /// reads as "set to nothing". The registry is re-read afterwards and the
    /// compiled instance dropped, so the next thing that needs the extension
    /// gets one built against the file as it now is — no restart.
    pub(crate) fn save_extension_settings(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        let Some(installed) = host.get(id) else { return };

        let mut values = std::collections::BTreeMap::new();
        if let Some(state) = self.settings_ui.as_ref() {
            for decl in installed.manifest.settings() {
                if decl.is_list() {
                    let Some(rows) = state.extensions.lists.get(&(id.to_string(), decl.key.to_string())) else {
                        continue;
                    };
                    let rows: Vec<std::collections::BTreeMap<String, String>> = rows
                        .iter()
                        .map(|row| {
                            row.iter().map(|(field, input)| (field.clone(), input.read(cx).text().trim().to_string())).collect()
                        })
                        .collect();
                    values.insert(decl.key.to_string(), serde_json::to_string(&rows).unwrap_or_default());
                    continue;
                }
                let Some(field) = state.extensions.field(id, decl.key.as_str()) else { continue };
                let text = field.read(cx).text().trim().to_string();
                if !text.is_empty() {
                    values.insert(decl.key.to_string(), text);
                }
            }
        }

        match crate::extensions::write_settings(&installed, &values) {
            Ok(()) => {
                host.reload(id);
                // Started again below, against what the file now says — how
                // far back to go may be the setting that just changed.
                self.scans.retain(|(extension, _), _| extension != id);
                self.scan_progress.retain(|(extension, _), _| extension != id);
                // What it draws may have changed with what it was told — a
                // label shown or hidden — so every row is asked again.
                host.forget_decorations();
                self.reload(cx);
                if let Some(state) = self.settings_ui.as_mut() {
                    state.extensions.edited.remove(id);
                }

                // What it needs now, which may be nothing.
                let missing = host
                    .get(id)
                    .map(|reloaded| reloaded.unconfigured().to_vec())
                    .unwrap_or_default();

                match missing.is_empty() {
                    true => {
                        self.say(format!("`{id}` saved"), cx);
                        // It may have been waiting on exactly this, so start
                        // it and draw whatever it contributes.
                        self.start_extensions(cx);
                    }
                    false => self.say(format!("`{id}` still needs {}", missing.join(", ")), cx),
                }
            }
            Err(e) => self.say(format!("Could not write settings for `{id}`: {e}"), cx),
        }
        cx.notify();
    }

    /// Adds a row to a list setting, with every column at its default.
    pub(crate) fn add_list_row(&mut self, extension: &str, key: &str, cx: &mut Context<Self>) {
        let Some(decl) = self
            .extensions
            .as_ref()
            .and_then(|host| host.get(extension))
            .and_then(|installed| installed.manifest.setting(key).cloned())
        else {
            return;
        };
        let row = list_row(&decl, &decl.blank_row(), cx);
        if let Some(state) = self.settings_ui.as_mut() {
            state.extensions.lists.entry((extension.to_string(), key.to_string())).or_default().push(row);
            state.extensions.edited.insert(extension.to_string());
        }
        cx.notify();
    }

    pub(crate) fn remove_list_row(&mut self, extension: &str, key: &str, index: usize, cx: &mut Context<Self>) {
        if let Some(state) = self.settings_ui.as_mut()
            && let Some(rows) = state.extensions.lists.get_mut(&(extension.to_string(), key.to_string()))
            && index < rows.len()
        {
            rows.remove(index);
            state.extensions.edited.insert(extension.to_string());
        }
        cx.notify();
    }

    /// Switches an extension on or off.
    ///
    /// Written to disk immediately and applied at the next launch. Tearing an
    /// instance down mid-session would mean doing it underneath whichever
    /// background thread happens to be inside it, which is not a thing to do
    /// for a checkbox — so the pane says so rather than pretending.
    pub(crate) fn set_extension_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = &self.extensions else { return };
        let root = host.root();

        match crate::extensions::set_enabled(&root, id, enabled) {
            Ok(()) => {
                self.extension_switches.insert(id.to_string(), enabled);
                cx.notify();
                self.say(
                match enabled {
                    true => format!("`{id}` is on at the next launch"),
                    false => format!("`{id}` is off at the next launch"),
                },
                cx,
            )
            }
            Err(e) => self.say(format!("Could not change `{id}`: {e}"), cx),
        }
    }

    /// Asks every contributed panel to describe itself again.
    ///
    /// On start, and after anything an extension did — a panel that counts
    /// what a command just changed should show the new number without being
    /// clicked.
    pub fn redraw_extension_panels(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        if host.panels().is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let changed =
                cx.background_executor().spawn(async move { host.draw_panels() }).await;
            if changed {
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        })
        .detach();
    }

    /// Asks the extensions what to draw on the messages now loaded.
    ///
    /// Called after a reload. Only messages with no answer yet are asked
    /// about, and the answer is cached even when it is empty — so scrolling a
    /// mailbox that has been decorated once costs nothing at all.
    pub fn decorate_messages(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        if !host.any_decorators() {
            return;
        }

        let messages: Vec<_> = self.envelopes.iter().take(DECORATE_BATCH).cloned().collect();
        if messages.is_empty() {
            return;
        }

        self.extensions_task = Some(cx.spawn(async move |this, cx| {
            let changed =
                cx.background_executor().spawn(async move { host.decorate(&messages) }).await;

            if changed {
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        }));
    }

    /// Tells the rules about mail that has just been committed.
    ///
    /// The ids come from the sync that stored them, so what a rule is handed
    /// is mail that certainly exists — which is the difference between a rule
    /// that can file something and one racing the store.
    pub fn extensions_on_messages(&mut self, ingested: Vec<MessageId>, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        if !host.any_rules() || ingested.is_empty() {
            return;
        }

        let store = self.store.clone();
        let scanning = self.scanning_map();

        cx.spawn(async move |this, cx| {
            let outcomes = cx
                .background_executor()
                .spawn(async move {
                    // Read back as envelopes, which is what the hook takes —
                    // and reading them here means a rule sees the message as
                    // the store has it rather than as the network described
                    // it. A message deleted in between is simply not there,
                    // and skipping it is right.
                    let envelopes: Vec<_> = ingested
                        .iter()
                        .filter_map(|id| store.envelope(*id).ok().flatten())
                        .collect();
                    host.on_messages(&envelopes, &scanning)
                })
                .await;

            this.update(cx, |this, cx| {
                this.apply_outcomes(outcomes, None, cx);
                // New mail scored after the list loaded has moved between
                // filters, and the counts above the list should say so.
                if this.active_scanner().is_some() {
                    this.reload(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The action the palette, a keystroke and a panel button all dispatch.
    ///
    /// One path from intent to behaviour, the same as everything else here: an
    /// extension's command is run by dispatching an action, whether it was
    /// picked from the palette, pressed as a key, or clicked on a button
    /// inside the extension's own panel.
    pub(crate) fn run_extension_command_action(
        &mut self,
        action: &RunExtensionCommand,
        _: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.run_extension_command(action.id.clone(), cx);
    }

    /// Runs an extension command, named as the palette names it.
    pub fn run_extension_command(&mut self, qualified: String, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };

        let context = CommandContext {
            selection: self.selection(),
            mailbox: match self.scope {
                Scope::Mailbox(id) => Some(id),
                _ => None,
            },
            // The account being looked at, whether it was chosen directly or
            // by way of one of its folders. An extension asking "whose mail is
            // this" should not have to care which.
            account: match self.scope {
                Scope::Account(id) => Some(id),
                Scope::Mailbox(id) => {
                    self.mailboxes.iter().find(|m| m.id == id).map(|m| m.account_id)
                }
                Scope::Everything => None,
            },
            search: self.search_text.clone(),
        };

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { host.run_command(&qualified, context) })
                .await;

            this.update(cx, |this, cx| this.apply_outcomes(vec![outcome], None, cx)).ok();
        })
        .detach();
    }

    /// Applies what a set of calls asked for.
    ///
    /// Moves are gathered and applied together rather than one at a time: that
    /// is what puts them on the undo stack as one entry, so an extension that
    /// files nine messages is one ⌘Z rather than nine.
    ///
    /// `touched` is the messages the calls were about, when that is known to
    /// be all they could have changed; `None` assumes anything may have.
    fn apply_outcomes(
        &mut self,
        outcomes: Vec<Outcome>,
        touched: Option<&[MessageId]>,
        cx: &mut Context<Self>,
    ) {
        let mut moves: Vec<(MessageId, MailboxId)> = Vec::new();
        let mut said: Option<String> = None;
        let mut by: Option<String> = None;

        for outcome in outcomes {
            if let Some(why) = outcome.failure {
                log::warn!("`{}` failed: {why}", outcome.extension);
                // Said out loud, not only logged. An extension that quietly
                // stops working is one somebody debugs by wondering.
                said = Some(format!("{}: {why}", outcome.extension));
                continue;
            }

            for effect in outcome.effects {
                if let Some(line) = self.apply_effect(&outcome.extension, effect, &mut moves, cx)
                {
                    said = Some(line);
                    by = Some(outcome.extension.clone());
                }
            }
        }

        if !moves.is_empty() {
            let verb = match &by {
                Some(extension) => format!("{extension} filed"),
                None => "Filed".to_string(),
            };
            self.apply_moves(moves, &verb, crate::workspace::Landing::Replacement, cx);
        }

        // An extension that has just run may answer differently about any
        // message now — scoring the selection is exactly that, and it returns
        // nothing but a status line to say so. The cache is dropped and the
        // visible rows asked again, rather than guessed at: a sweep scores
        // messages that were never selected, so there is no set of ids here
        // that would be the right one.
        if let Some(host) = &self.extensions {
            match touched {
                Some(messages) => host.forget_decorations_of(messages),
                None => host.forget_decorations(),
            }
        }
        self.decorate_messages(cx);

        if let Some(line) = said {
            self.say(line, cx);
        }

        // Whatever a command changed, a panel counting it is now out of date.
        self.redraw_extension_panels(cx);
        cx.notify();
    }

    /// One effect. Returns a line for the status bar where it produced one.
    fn apply_effect(
        &mut self,
        extension: &str,
        effect: Effect,
        moves: &mut Vec<(MessageId, MailboxId)>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        match effect {
            Effect::SetFlags { message, add, remove } => {
                let envelope = self.envelopes.iter().find(|e| e.id == message)?;
                // A flag in both is turned off, which is the more conservative
                // reading and the one the contract promises.
                let next = Flags((envelope.flags.0 | add.0) & !remove.0);
                let ids = envelope.message_ids();
                self.update_flags(message, &ids, next, cx);
                None
            }
            Effect::MoveTo { message, mailbox } => {
                moves.push((message, mailbox));
                None
            }
            Effect::Archive(message) => {
                let target = self.role_mailbox_for(message, MailboxRole::Archive)?;
                moves.push((message, target));
                None
            }
            Effect::Trash(message) => {
                let target = self.role_mailbox_for(message, MailboxRole::Trash)?;
                moves.push((message, target));
                None
            }
            Effect::Notify { title, body, message: _ } => {
                // The message an extension wanted opened goes unused: the
                // notification path takes a title and a body and has no way to
                // carry a click target yet. Dropped rather than faked.
                crate::notify::post(&[crate::notify::Banner { title, body }]);
                None
            }
            Effect::Status(line) => Some(line),
            Effect::OpenUrl(url) => {
                // Only what a link in a message could open. An extension that
                // could hand the platform a `file://` would be an extension
                // with a filesystem after all.
                match url.starts_with("https://") || url.starts_with("http://") {
                    true => cx.open_url(&url),
                    false => log::warn!("`{extension}` tried to open `{url}`"),
                }
                None
            }
            Effect::InvalidatePanel(_) => {
                // A panel is drawn from whatever the extension returns the next
                // time it is asked, and asking again is a redraw.
                cx.notify();
                None
            }
        }
    }

    /// A mailbox as the settings file names it: `(address, path)`.
    fn mailbox_key(&self, id: MailboxId) -> Option<(String, String)> {
        let mailbox = self.mailboxes.iter().find(|m| m.id == id)?;
        let account = self.accounts.iter().find(|a| a.id == mailbox.account_id)?;
        Some((account.email.clone(), mailbox.path.clone()))
    }

    /// Whether an extension is switched on for a mailbox.
    pub(crate) fn is_scanning(&self, extension: &str, mailbox: MailboxId) -> bool {
        self.mailbox_key(mailbox)
            .is_some_and(|(email, path)| self.settings.is_scanning(extension, &email, &path))
    }

    /// The mailbox an extension's switch applies to in the current view.
    ///
    /// The account row lists that account's inbox, so it is the inbox here
    /// too: a switch turned on in one is on in the other.
    pub(crate) fn scanned_mailbox(&self) -> Option<MailboxId> {
        match self.scope {
            Scope::Mailbox(id) => Some(id),
            Scope::Account(account) => self
                .mailboxes
                .iter()
                .find(|m| m.account_id == account && m.role == MailboxRole::Inbox)
                .map(|m| m.id),
            Scope::Everything => None,
        }
    }

    /// The extension switched on for the mailbox being looked at, if any.
    pub(crate) fn active_scanner(&self) -> Option<String> {
        let mailbox = self.scanned_mailbox()?;
        let host = self.extensions.as_ref()?;
        host.scanners()
            .into_iter()
            .map(|installed| installed.id().to_string())
            .find(|extension| self.is_scanning(extension, mailbox))
    }

    /// Every switched-on mailbox, by extension, as the store's ids.
    pub(crate) fn scanning_map(&self) -> HashMap<String, HashSet<MailboxId>> {
        let mut map: HashMap<String, HashSet<MailboxId>> = HashMap::new();
        for on in &self.settings.scanning {
            let found = self.mailboxes.iter().find(|m| {
                m.path == on.path
                    && self
                        .accounts
                        .iter()
                        .any(|a| a.id == m.account_id && a.email.eq_ignore_ascii_case(&on.account))
            });
            if let Some(mailbox) = found {
                map.entry(on.extension.clone()).or_default().insert(mailbox.id);
            }
        }
        map
    }

    /// Flips an extension's switch for the mailbox being looked at.
    ///
    /// On hands it the mailbox's backlog straight away; off stops any of that
    /// still under way, and keeps what was already worked out.
    pub(crate) fn set_scanning(&mut self, extension: &str, on: bool, cx: &mut Context<Self>) {
        let Some(mailbox) = self.scanned_mailbox() else { return };
        let Some((email, path)) = self.mailbox_key(mailbox) else { return };

        self.settings.set_scanning(extension, &email, &path, on);
        self.persist_settings(cx);

        let key = (extension.to_string(), mailbox);
        match on {
            true => self.start_scan(extension.to_string(), mailbox, cx),
            false => {
                self.scans.remove(&key);
                self.scan_progress.remove(&key);
                self.label_filter = None;
                self.label_counts.clear();
            }
        }
        self.reload(cx);
    }

    /// Narrows the list to one of an extension's filters, or widens it back.
    pub(crate) fn set_label_filter(&mut self, filter: Option<(String, String)>, cx: &mut Context<Self>) {
        if self.label_filter == filter {
            return;
        }
        self.label_filter = filter;
        self.selected = 0;
        self.chosen = None;
        self.reload(cx);
    }

    /// Starts the backlog of every switched-on mailbox that is not already
    /// being worked through.
    ///
    /// At launch, because a backlog cut short by quitting is picked up where
    /// it stopped — an extension skips what it has seen, so starting over
    /// costs a lookup per message — and after an extension's settings are
    /// saved, because how far back to go may have changed.
    fn resume_scans(&mut self, cx: &mut Context<Self>) {
        for (extension, mailboxes) in self.scanning_map() {
            for mailbox in mailboxes {
                if !self.scans.contains_key(&(extension.clone(), mailbox)) {
                    self.start_scan(extension.clone(), mailbox, cx);
                }
            }
        }
    }

    /// Hands an extension a mailbox's backlog, a batch at a time.
    ///
    /// As far back as the setting its switch names, newest first so what
    /// someone is most likely to look at is done first. Stops when the
    /// backlog runs out, when a call fails outright, or when the switch is
    /// turned off — which drops this task.
    fn start_scan(&mut self, extension: String, mailbox: MailboxId, cx: &mut Context<Self>) {
        let Some(host) = self.extensions.clone() else { return };
        let Some(installed) = host.get(&extension) else { return };
        let span = installed
            .manifest
            .scan
            .as_ref()
            .and_then(|scan| scan.since.as_ref())
            .and_then(|key| installed.config.get(key.as_str()))
            .and_then(span_seconds);
        // A switch with no reach backwards only covers new mail.
        let Some(span) = span else { return };

        let store = self.store.clone();
        let key = (extension.clone(), mailbox);
        let name = installed.manifest.name.clone();
        let folder = self.mailboxes.iter().find(|m| m.id == mailbox).map(|m| m.name.clone()).unwrap_or_default();
        let since = now() - span;

        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let backlog = cx
                    .background_executor()
                    .spawn(async move {
                        store.envelopes(Scope::Mailbox(mailbox), &Query::All, crate::workspace::LIST_LIMIT)
                    })
                    .await;
                let backlog: Vec<Envelope> = match backlog {
                    Ok(all) => all.into_iter().filter(|e| e.date >= since).collect(),
                    Err(e) => {
                        log::warn!("`{extension}` could not read {folder}'s backlog: {e:#}");
                        Vec::new()
                    }
                };

                let total = backlog.len();
                let mut done = 0;
                let mut stopped = None;
                for batch in backlog.chunks(SCAN_BATCH) {
                    let batch = batch.to_vec();
                    let ids: Vec<MessageId> = batch.iter().map(|e| e.id).collect();
                    let (host, extension) = (host.clone(), extension.clone());
                    let outcome = cx
                        .background_executor()
                        .spawn(async move { host.scan(&extension, &batch) })
                        .await;
                    done += ids.len();
                    stopped = outcome.failure.clone();

                    let applied = this.update(cx, |this, cx| {
                        this.scan_progress.insert(key.clone(), (done, total));
                        this.apply_outcomes(vec![outcome], Some(&ids), cx);
                        // The counts above the list, brought up to date.
                        if this.scanned_mailbox() == Some(mailbox) {
                            this.reload(cx);
                        }
                    });
                    if applied.is_err() || stopped.is_some() {
                        break;
                    }
                }

                this.update(cx, |this, cx| {
                    this.scan_progress.remove(&key);
                    if stopped.is_none() && total > 0 {
                        this.say(format!("{name} has been through {total} in {folder}"), cx);
                    }
                    cx.notify();
                    // Last, not first: this is the task being dropped, from
                    // inside itself.
                    this.scans.remove(&key);
                })
                .ok();
            }
        });
        self.scans.insert(key, task);
    }

    /// The mailbox of a given role in the account a message belongs to.
    ///
    /// Per message rather than per window: archiving from the unified view
    /// must not file work mail into a personal folder.
    fn role_mailbox_for(&self, message: MessageId, role: MailboxRole) -> Option<MailboxId> {
        let envelope = self.envelopes.iter().find(|e| e.id == message)?;
        self.mailboxes
            .iter()
            .find(|m| m.account_id == envelope.account_id && m.role == role)
            .map(|m| m.id)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The fields for one row of a list setting, filled from `row`.
fn list_row(
    decl: &spool_ext::manifest::ConfigDecl,
    row: &std::collections::BTreeMap<String, String>,
    cx: &mut Context<Workspace>,
) -> std::collections::BTreeMap<String, gpui::Entity<crate::text_input::TextInput>> {
    let row = decl.complete_row(row);
    decl.fields
        .iter()
        .map(|field| {
            let value = row.get(field.key.as_str()).cloned().unwrap_or_default();
            let input = cx.new(|cx| {
                let mut input = crate::text_input::TextInput::new(field.title.clone(), cx);
                if !value.is_empty() {
                    input.set_content(value, cx);
                }
                input
            });
            (field.key.to_string(), input)
        })
        .collect()
}
