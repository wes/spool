//! A worked example: what unread mail is there, and what is taking up room.
//!
//! Small on purpose, but it touches every surface an extension has — a rule
//! that runs on new mail, a badge on list rows, a command in the palette, and a
//! panel of its own — so it doubles as the thing to copy when starting a real
//! one.
//!
//! Note what it does *not* ask for. It never files or flags anything, so its
//! manifest has no `modify-mail`, and the host refuses to start it if the code
//! ever drifts into wanting it. That check is worth having on purpose: an
//! extension's manifest is the only thing a user reads before trusting it.

use spool_extension::{
    CommandContext, Decorations, Effect, Emphasis, Envelope, Extension, Filter, Notification,
    Query, Scope, Tree, export_extension, host, ui,
};

/// What counts as a message worth mentioning the size of.
///
/// Four megabytes: past what anybody attaches by accident and well short of
/// what a mail server will usually take, so the badge means "this one is
/// unusual" rather than appearing on half the list.
const BIG: u32 = 4 * 1024 * 1024;

struct InboxStats;

impl Extension for InboxStats {
    fn init() -> Result<(), String> {
        host::log(host::Level::Info, "inbox-stats is watching");
        Ok(())
    }

    /// New mail: say something only about the mail worth saying something
    /// about.
    ///
    /// One notification for the batch rather than one per message. A sync that
    /// pulls four hundred messages would otherwise be four hundred
    /// notifications, which is how an extension gets uninstalled.
    fn on_messages(messages: Vec<Envelope>) -> Vec<Effect> {
        let big: Vec<&Envelope> = messages.iter().filter(|m| m.size >= BIG).collect();

        let Some(first) = big.first() else {
            return Vec::new();
        };

        let title = match big.len() {
            1 => format!("{} sent {}", sender(first), size(first.size)),
            n => format!("{n} large messages arrived"),
        };

        vec![Effect::Notify(Notification {
            title,
            body: first.subject.clone(),
            // Clicking it opens the message it is about, which is the only
            // thing a notification about one message should do.
            message: Some(first.id),
        })]
    }

    /// The size of a message, on the row, when the size is the surprising part.
    ///
    /// Pure, and it has to be: the host calls this off the UI thread, caches
    /// what comes back, and draws rows from the cache.
    fn decorate(message: Envelope) -> Decorations {
        let mut badges = Vec::new();
        if message.size >= BIG {
            badges.push(ui::badge(size(message.size), Emphasis::Muted));
        }

        Decorations { banners: Vec::new(), badges }
    }

    fn run_command(id: String, context: CommandContext) -> Vec<Effect> {
        match id.as_str() {
            "summarise" => vec![Effect::Status(summary(&context))],
            other => {
                // Unreachable while the manifest and this agree, which is
                // exactly why it is worth a line in the log when they do not.
                host::log(host::Level::Warn, &format!("no command called `{other}`"));
                Vec::new()
            }
        }
    }

    fn render_panel(_id: String) -> Tree {
        let mut panel = ui::Builder::new();
        let mut rows = vec![panel.text("Unread", Emphasis::Heading)];

        for account in host::accounts() {
            let unread = count(Scope::Account(account.id), Filter::Unread);

            let name = panel.text(account.email.clone(), Emphasis::Normal);
            let gap = panel.spacer();
            let number = panel.text(unread.to_string(), Emphasis::Accent);
            rows.push(panel.row([name, gap, number]));
        }

        rows.push(panel.divider());
        rows.push(panel.button("Summarise", "summarise"));

        let root = panel.column(rows);
        panel.finish(root)
    }
}

/// One line for the status bar.
fn summary(context: &CommandContext) -> String {
    // The scope the user is actually looking at, so the number answers the
    // question they asked rather than a more general one.
    let scope = match context.mailbox {
        Some(mailbox) => Scope::Mailbox(mailbox),
        None => match context.account {
            Some(account) => Scope::Account(account),
            None => Scope::Everything,
        },
    };

    let unread = count(scope, Filter::Unread);
    let flagged = count(scope, Filter::Flagged);

    match unread {
        0 => format!("Nothing unread \u{2014} {flagged} flagged"),
        1 => format!("1 unread, {flagged} flagged"),
        n => format!("{n} unread, {flagged} flagged"),
    }
}

/// How many messages match, up to the host's own cap.
///
/// The cap is why this says "up to": the host will not hand over an unbounded
/// list, so a count taken this way saturates rather than growing forever. Good
/// enough for a readout, and the alternative is a counting call that does not
/// exist yet.
fn count(scope: Scope, filter: Filter) -> usize {
    host::query_envelopes(&Query { scope, filter, limit: 1_000 }).len()
}

fn sender(message: &Envelope) -> String {
    match message.from.name.trim().is_empty() {
        true => message.from.addr.clone(),
        false => message.from.name.clone(),
    }
}

fn size(bytes: u32) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MB)
}

export_extension!(InboxStats);
