//! Writing to a group of clients at once.
//!
//! One message per person, each addressed to them alone. A single message
//! with forty people on it — even forty blind copies — reads as a circular,
//! lands in Promotions, and is one reply-all away from telling every client
//! who the others are. Forty messages each to one person read as what they
//! are: a note from you.
//!
//! The message can say each person's name. `{first_name}`, `{name}`,
//! `{company}` and `{email}` are filled in per recipient, and `{first_name|there}`
//! says what to write for somebody whose name the mail never gave — the sheet
//! shows who that is before anything goes, rather than leaving it to the "Hi ,"
//! that arrives in their inbox.
//!
//! They go on one login, a little apart. A shared host counts logins and
//! messages a minute, and forty of either at once from one address is exactly
//! what its firewall is for; the run stops at the first sign the server wants
//! it to slow down, and says how far it got. The copies are filed in Sent the
//! way any sent message is, which is also what puts each one in that client's
//! history.
//!
//! It is written in the composer — the same window as any message, with the
//! To line given over to the group (see `compose::group`) — and goes the way
//! the composer sends: text, or HTML with a text version beside it, signed,
//! with whatever is attached.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{AnyElement, App, Context, FontWeight, SharedString, div, prelude::*, px};
use spool_core::{
    Address, address,
    draft::{Draft, Format},
    markup,
};
use spool_store::Store;

use crate::{
    chrome::Tip,
    clients::{ClientBook, Entry},
    clients_window::{ClientsWindow, Sheet, chip},
    theme::Theme,
};

/// The most one sending will take.
///
/// Gmail lets an ordinary account send five hundred messages a day, and other
/// providers fewer. A group larger than that is a newsletter, and a newsletter
/// wants a service built for one — unsubscribe links, bounce handling — not a
/// mail client sending as you.
pub(crate) const MOST: usize = 500;

/// How long to wait between two messages.
///
/// About a minute for fifty, which is nothing to somebody sending a note and
/// well under what any provider calls a burst.
const PACE: Duration = Duration::from_millis(1_200);

/// What a message can be filled in with, as the sheet offers them.
pub(crate) const FIELDS: [&str; 4] = ["{first_name}", "{name}", "{company}", "{email}"];

/// Somebody a message to the group would go to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recipient {
    /// Their name as they have been known, or `None` when all the mail ever
    /// gave was an address.
    pub name: Option<String>,
    pub addr: String,
    pub company: String,
    /// Whether they are getting it. Somebody left out is still listed, so a
    /// box can be ticked back.
    pub included: bool,
    /// Why they were left out to begin with, when it was not by hand.
    pub why_not: Option<&'static str>,
}

/// Who a group is, as recipients.
///
/// Everyone once, by address. Somebody with no address — a company known only
/// by its domain — is listed and cannot be ticked; somebody whose mail looks
/// automated is listed and starts unticked, because a `noreply@` that gets
/// "Hi there" back is a bounce at best.
pub(crate) fn recipients<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<Recipient> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for entry in entries {
        let addr = entry.addrs.first().map(|a| address::normalize(a));
        if addr.as_ref().is_some_and(|a| !seen.insert(a.clone())) {
            continue;
        }
        let why_not = match &addr {
            None => Some("No address to write to"),
            Some(addr) if entry.automated || crate::clients::is_automated(addr) => Some("Looks automated"),
            Some(_) => None,
        };
        out.push(Recipient {
            name: real_name(entry),
            addr: addr.unwrap_or_default(),
            company: entry.company.to_string(),
            included: why_not.is_none(),
            why_not,
        });
    }
    out
}

/// A name somebody was actually given, rather than one the directory made up.
///
/// The directory shows the front of the address for somebody whose mail never
/// carried a name, which is right for a list and wrong for "Hi den,".
fn real_name(entry: &Entry) -> Option<String> {
    let name = entry.name.trim();
    let local = entry.addrs.first().and_then(|a| a.split('@').next()).unwrap_or_default();
    let made_up = name.is_empty()
        || name.contains('@')
        || name == "Unnamed"
        || name.eq_ignore_ascii_case(local)
        || entry.domains.iter().any(|d| name.eq_ignore_ascii_case(d));
    (!made_up).then(|| name.to_string())
}

/// What somebody is called to their face: the first word of their name, or
/// the word after the comma of a "Ruiz, Ana".
pub(crate) fn first_name(name: &str) -> Option<String> {
    let name = match name.split_once(',') {
        Some((_, first)) if !first.trim().is_empty() => first,
        _ => name,
    };
    let word = name.split_whitespace().next()?.trim_matches(|c: char| !c.is_alphanumeric());
    (!word.is_empty()).then(|| word.to_string())
}

/// A field's value for one recipient, if they have one.
fn value(field: &str, recipient: &Recipient) -> Option<Option<String>> {
    let known = match field {
        "first_name" | "firstname" | "first" => recipient.name.as_deref().and_then(first_name),
        "name" => recipient.name.clone(),
        "company" => Some(recipient.company.clone()).filter(|c| !c.is_empty()),
        "email" => Some(recipient.addr.clone()),
        // Not a field: written as it was.
        _ => return None,
    };
    Some(known)
}

/// The fields in a template, each with its fallback, in order.
fn fields(template: &str) -> Vec<(String, Option<String>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = template[from..].find('{').map(|i| from + i) {
        let Some(close) = template[open..].find('}').map(|i| open + i) else { break };
        let inner = &template[open + 1..close];
        // A brace that opens again before it closes was not a field.
        if inner.contains('{') {
            from = open + 1;
            continue;
        }
        let (key, fallback) = match inner.split_once('|') {
            Some((key, fallback)) => (key, Some(fallback.to_string())),
            None => (inner, None),
        };
        out.push((key.trim().to_ascii_lowercase(), fallback, open..close + 1));
        from = close + 1;
    }
    out
}

/// A template, filled in for one person.
///
/// A field they have no value for takes its fallback, or nothing. Anything in
/// braces that is not a field is left as it was written.
pub(crate) fn fill(template: &str, recipient: &Recipient) -> String {
    fill_as(template, recipient, false)
}

/// As [`fill`], into HTML. What is filled in is escaped, so a company called
/// "Ruiz & Sons" is that and not the start of an entity. A fallback is left
/// as written: it was typed into the HTML, so it is HTML already.
pub(crate) fn fill_html(template: &str, recipient: &Recipient) -> String {
    fill_as(template, recipient, true)
}

fn fill_as(template: &str, recipient: &Recipient, html: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut at = 0;
    for (key, fallback, range) in fields(template) {
        let Some(known) = value(&key, recipient) else { continue };
        out.push_str(&template[at..range.start]);
        match (known, fallback) {
            (Some(known), _) if html => out.push_str(&markup::escape(&known)),
            (Some(known), _) => out.push_str(&known),
            (None, Some(fallback)) => out.push_str(&fallback),
            (None, None) => {}
        }
        at = range.end;
    }
    out.push_str(&template[at..]);
    out
}

/// Whether filling a template in for somebody leaves a hole: a field they
/// have no value for, with no fallback to stand in.
pub(crate) fn leaves_a_gap(template: &str, recipient: &Recipient) -> bool {
    fields(template)
        .into_iter()
        .any(|(key, fallback, _)| matches!(value(&key, recipient), Some(None)) && fallback.is_none())
}

/// A sending under way, or just finished.
///
/// Held by the book rather than the window, so closing the window does not
/// stop it, and opening it again shows how far it has got.
pub struct Mailing {
    pub subject: String,
    pub body: String,
    /// The message as HTML, when it went as HTML.
    pub html: Option<String>,
    pub attachments: Vec<spool_core::draft::Attached>,
    pub from: String,
    pub group: String,
    /// Everybody it was sent to, in order.
    pub recipients: Vec<Recipient>,
    pub sent: usize,
    /// Who it did not reach, and why.
    pub failed: Vec<(String, String)>,
    /// Why it stopped before the end, if it did.
    pub stopped: Option<String>,
    /// What went wrong filing the copies in Sent, if anything did.
    pub filing: Option<String>,
    pub finished: bool,
    stop: Arc<AtomicBool>,
}

impl Mailing {
    /// How many it has dealt with, either way.
    pub fn done(&self) -> usize {
        self.sent + self.failed.len()
    }

    /// Asks it to stop after the message it is on.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// The status line's sentence.
    pub fn summary(&self) -> String {
        let total = self.recipients.len();
        match self.finished {
            false => format!("Sending \u{201c}{}\u{201d}: {} of {total}", self.subject, self.done()),
            true => match self.failed.len() {
                0 => format!("Sent \u{201c}{}\u{201d} to {} {}", self.subject, self.sent, people(self.sent)),
                failed => format!("Sent \u{201c}{}\u{201d} to {} of {total}; {failed} did not go", self.subject, self.sent),
            },
        }
    }
}

fn people(n: usize) -> &'static str {
    if n == 1 { "person" } else { "people" }
}

/// One thing the run has to report.
enum Step {
    Sent,
    Failed(String, String),
    Stopped(String),
    Filed(Option<String>),
}

/// What is sent, decided on the UI thread before any of it leaves.
///
/// The message as the composer has it: signed already, and with `{fields}`
/// still in it to fill in for each person.
pub(crate) struct Job {
    pub from: Address,
    pub subject: String,
    pub body: String,
    /// Sent as HTML, with `body` as its text version, when set.
    pub html: Option<String>,
    pub attachments: Vec<spool_core::draft::Attached>,
    pub recipients: Vec<Recipient>,
}

/// Starts sending, unless something is already being sent.
pub(crate) fn start(job: Job, group: String, store: Store, cx: &mut App) -> Result<(), String> {
    let Some(clients) = cx.try_global::<ClientBook>() else { return Err("Clients is not running".into()) };
    if clients.mailing.as_ref().is_some_and(|m| !m.finished) {
        return Err("Another message to a group is still being sent".into());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mailing = Mailing {
        subject: job.subject.clone(),
        body: job.body.clone(),
        html: job.html.clone(),
        attachments: job.attachments.clone(),
        from: job.from.addr.clone(),
        group,
        recipients: job.recipients.clone(),
        sent: 0,
        failed: Vec::new(),
        stopped: None,
        filing: None,
        finished: false,
        stop: stop.clone(),
    };
    cx.update_global::<ClientBook, _>(|clients, _| clients.mailing = Some(mailing));

    let (tx, rx) = smol::channel::unbounded::<Step>();
    cx.background_executor().spawn(run(job, store, tx, stop)).detach();
    cx.spawn(async move |cx| {
        while let Ok(step) = rx.recv().await {
            let finished = matches!(step, Step::Filed(_));
            let _ = cx.update(|cx| {
                if !cx.has_global::<ClientBook>() {
                    return;
                }
                cx.update_global::<ClientBook, _>(|clients, _| {
                    let Some(mailing) = clients.mailing.as_mut() else { return };
                    match step {
                        Step::Sent => mailing.sent += 1,
                        Step::Failed(addr, why) => mailing.failed.push((addr, why)),
                        Step::Stopped(why) => mailing.stopped = Some(why),
                        Step::Filed(problem) => {
                            mailing.filing = problem;
                            mailing.finished = true;
                        }
                    }
                });
                if finished {
                    // The copies are in Sent, which is in the index: each
                    // client's history now has their message in it.
                    crate::clients::mail_changed(cx);
                }
            });
            if finished {
                break;
            }
        }
    })
    .detach();
    Ok(())
}

/// Whether an address is one of the accounts this copy of Spool sends as —
/// the same question the run asks of the same file before it logs in.
pub(crate) fn can_send_as(addr: &str) -> bool {
    spool_net::Config::load(spool_net::Config::default_path())
        .is_ok_and(|config| config.accounts.iter().any(|a| a.email.eq_ignore_ascii_case(addr)))
}

/// One person's copy of the message, filled in and signed.
fn copy_for(job: &Job, recipient: &Recipient) -> Draft {
    let mut draft = Draft::new(job.from.clone());
    draft.to = vec![Address::new(recipient.name.clone().unwrap_or_default(), recipient.addr.clone())];
    draft.subject = fill(&job.subject, recipient);
    draft.attachments = job.attachments.clone();
    match &job.html {
        // As the composer sends HTML: the formatting, and the same words as
        // text beside it.
        Some(html) => {
            let filled = fill_html(html, recipient);
            draft.format = Format::Html;
            draft.body = markup::to_text(&filled);
            draft.html = Some(filled);
        }
        None => draft.body = fill(&job.body, recipient),
    }
    draft
}

/// Sends a job, one message at a time, and files what went.
async fn run(job: Job, store: Store, tx: smol::channel::Sender<Step>, stop: Arc<AtomicBool>) {
    let say = |step: Step| {
        let _ = tx.try_send(step);
    };

    let account = spool_net::Config::load(spool_net::Config::default_path()).ok().and_then(|config| {
        config.accounts.into_iter().find(|a| a.email.eq_ignore_ascii_case(&job.from.addr))
    });
    let Some(account) = account else {
        say(Step::Stopped(format!("{} is not set up to send from", job.from.addr)));
        say(Step::Filed(None));
        return;
    };
    let credentials = match account.credentials().await {
        Ok(credentials) => credentials,
        Err(e) => {
            say(Step::Stopped(format!("No password for {}: {e:#}", account.email)));
            say(Step::Filed(None));
            return;
        }
    };
    let mut session = match spool_net::smtp::Session::open(&account, &credentials).await {
        Ok(session) => Some(session),
        Err(e) => {
            say(Step::Stopped(format!("Could not log in to send: {e:#}")));
            say(Step::Filed(None));
            return;
        }
    };

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut filed: Vec<(String, i64)> = Vec::new();

    for (i, recipient) in job.recipients.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            say(Step::Stopped("Stopped".into()));
            break;
        }
        if i > 0 {
            smol::Timer::after(PACE).await;
        }
        // The connection dropped on the one before: a fresh one for the rest.
        let open = match session.as_mut() {
            Some(open) => open,
            None => match spool_net::smtp::Session::open(&account, &credentials).await {
                Ok(fresh) => session.insert(fresh),
                Err(e) => {
                    say(Step::Stopped(format!("Lost the connection and could not log in again: {e:#}")));
                    break;
                }
            },
        };

        let draft = copy_for(&job, recipient);
        let now = crate::clock::now_seconds();
        let message_id = spool_net::compose::message_id(&job.from, &format!("{unique:x}.{}.{i}", std::process::id()));
        use spool_net::compose::Copy;
        let message = spool_net::compose::render_with(&draft, &message_id, now, Copy::Sent);

        match open.send(&job.from.addr, std::slice::from_ref(&recipient.addr), &message).await {
            Ok(()) => {
                filed.push((spool_net::compose::render_with(&draft, &message_id, now, Copy::Filed), now));
                say(Step::Sent);
            }
            Err(e @ spool_net::smtp::SendError::Broken(_)) => {
                // Not sent again: the connection went somewhere between
                // handing it over and hearing back, and a second copy is
                // worse than a question.
                say(Step::Failed(recipient.addr.clone(), format!("The connection dropped while sending, so it may not have arrived ({e})")));
                session = None;
            }
            Err(e) if e.is_temporary() => {
                say(Step::Failed(recipient.addr.clone(), e.to_string()));
                say(Step::Stopped(format!("The server asked to wait before sending more ({e})")));
                break;
            }
            Err(e) => say(Step::Failed(recipient.addr.clone(), e.to_string())),
        }
    }

    if let Some(session) = session {
        session.quit().await;
    }

    let copies: Vec<(&str, i64)> = filed.iter().map(|(message, at)| (message.as_str(), *at)).collect();
    let filing = match copies.is_empty() {
        true => Ok(()),
        false => crate::compose::file_sent_copies(&store, &account, &credentials, &copies).await,
    };
    say(Step::Filed(filing.err().map(|e| format!("{e:#}"))));
}

/// Brings the progress of a sending forward in the Clients window, which is
/// where it is reported and where it can be stopped.
pub(crate) fn show_progress(cx: &mut App) {
    let Some(window) = cx.try_global::<ClientBook>().and_then(|c| c.window) else { return };
    window
        .update(cx, |clients, _, cx| {
            clients.sheet = Some(Sheet::Mailing);
            cx.notify();
        })
        .ok();
}

impl ClientsWindow {
    /// Opens a composer to write to everyone in `entries`.
    pub(crate) fn open_mail(&mut self, group: String, entries: Vec<Entry>, cx: &mut Context<Self>) {
        // A sending under way or just finished is what this shows, until it
        // is put away: one at a time.
        if cx.try_global::<ClientBook>().is_some_and(|c| c.mailing.is_some()) {
            self.sheet = Some(Sheet::Mailing);
            cx.notify();
            return;
        }

        let recipients = recipients(&entries);
        // From the book being looked at, or else the account most of them
        // have mail in: the address they know you by.
        let from = self.book_filter.as_ref().map(|b| b.email.clone()).or_else(|| {
            let mut counts: std::collections::HashMap<spool_core::AccountId, usize> = Default::default();
            for account in entries.iter().flat_map(|e| e.accounts.iter()) {
                *counts.entry(*account).or_default() += 1;
            }
            let busiest = counts.into_iter().max_by_key(|(_, n)| *n).map(|(id, _)| id)?;
            self.account_email(busiest).map(str::to_string)
        });
        let Some(from) = self.sender(from.as_deref()) else {
            self.status = "No account to send from".into();
            cx.notify();
            return;
        };

        // A greeting to fill in, room to write, and the signature, as a new
        // message in the composer starts.
        let signature = crate::compose::signature_of(&from.addr);
        let mut draft = Draft::new(from);
        draft.body = format!("Hi {{first_name|there}},\n\n{}", spool_core::draft::sign("", &signature));
        self.open_group_composer(crate::compose::Group::new(group, recipients), draft, cx);
    }

    /// Writes again to the ones it did not reach, with the same message.
    fn retry_failed(&mut self, cx: &mut Context<Self>) {
        let Some(mailing) = cx.try_global::<ClientBook>().and_then(|c| c.mailing.as_ref()) else { return };
        let failed: HashSet<&str> = mailing.failed.iter().map(|(addr, _)| addr.as_str()).collect();
        let recipients: Vec<Recipient> =
            mailing.recipients.iter().filter(|r| failed.contains(r.addr.as_str())).cloned().collect();
        let Some(from) = self.sender(Some(&mailing.from)) else { return };
        let mut draft = Draft::new(from);
        draft.subject = mailing.subject.clone();
        draft.body = mailing.body.clone();
        draft.format = if mailing.html.is_some() { Format::Html } else { Format::Plain };
        draft.html = mailing.html.clone();
        draft.attachments = mailing.attachments.clone();
        let group = mailing.group.clone();

        cx.update_global::<ClientBook, _>(|clients, _| clients.mailing = None);
        self.sheet = None;
        self.open_group_composer(crate::compose::Group::new(group, recipients), draft, cx);
    }

    /// An account to send as, by address: the one asked for, or the first.
    fn sender(&self, addr: Option<&str>) -> Option<Address> {
        let account = addr
            .and_then(|addr| self.accounts.iter().find(|a| a.email.eq_ignore_ascii_case(addr)))
            .or_else(|| self.accounts.first())?;
        Some(Address::new(account.name.clone(), account.email.clone()))
    }

    fn open_group_composer(&mut self, group: crate::compose::Group, draft: Draft, cx: &mut Context<Self>) {
        let opened = self.workspace.update(cx, |workspace, cx| workspace.open_group_composer(group, draft, cx));
        if opened.is_err() {
            self.status = "The mailbox window is gone, and the composer opens from it".into();
            cx.notify();
        }
    }

    pub(crate) fn render_mailing(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(mailing) = cx.try_global::<ClientBook>().and_then(|c| c.mailing.as_ref()) else {
            return div().into_any_element();
        };
        let total = mailing.recipients.len().max(1);
        let fraction = mailing.done() as f32 / total as f32;
        let finished = mailing.finished;
        let failed = mailing.failed.clone();
        let stopped = mailing.stopped.clone();
        let filing = mailing.filing.clone();
        let summary = mailing.summary();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(summary)))
            .child(
                div().h(px(4.)).w_full().rounded_full().bg(theme.border).overflow_hidden().child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded_full()
                        .bg(if failed.is_empty() { theme.accent } else { theme.yellow }),
                ),
            )
            .children((!finished).then(|| {
                div().text_size(theme.small_text_size).text_color(theme.text_faint).child(
                    "A little apart, so the server does not take it for a flood. \
                     Closing this window does not stop it; quitting Spool does.",
                )
            }))
            .children(stopped.map(|why| div().text_size(theme.small_text_size).text_color(theme.red).child(SharedString::from(why))))
            .children(filing.map(|why| {
                div().text_size(theme.small_text_size).text_color(theme.yellow).child(SharedString::from(format!(
                    "Sent, but the copies could not all be filed in Sent: {why}"
                )))
            }))
            .children((!failed.is_empty()).then(|| {
                div()
                    .id("mail-failed")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .child(caption(format!("Did not go \u{2014} {}", failed.len()), theme))
                    .children(failed.iter().enumerate().map(|(i, (addr, why))| {
                        let why: SharedString = why.clone().into();
                        div()
                            .id(("mail-failed-row", i))
                            .flex()
                            .gap_2()
                            .text_size(theme.small_text_size)
                            .child(div().w(px(200.)).flex_shrink_0().truncate().child(SharedString::from(addr.clone())))
                            .child(div().flex_1().min_w_0().truncate().text_color(theme.text_faint).child(why.clone()))
                            .tooltip(move |_, cx| cx.new(|_| Tip { text: why.clone() }).into())
                    }))
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .when(!finished, |row| {
                        row.child(chip("mail-hide", "Hide", false, theme).on_click(cx.listener(|this, _, _, cx| {
                            this.sheet = None;
                            cx.notify();
                        })))
                        .child(chip("mail-stop", "Stop", true, theme).on_click(cx.listener(|_, _, _, cx| {
                            if let Some(mailing) = cx.try_global::<ClientBook>().and_then(|c| c.mailing.as_ref()) {
                                mailing.stop();
                            }
                        })))
                    })
                    .when(finished && !failed.is_empty(), |row| {
                        row.child(
                            chip("mail-retry", format!("Write to the {} again", failed.len()), false, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.retry_failed(cx))),
                        )
                    })
                    .when(finished, |row| {
                        row.child(chip("mail-done", "Done", true, theme).on_click(cx.listener(|this, _, _, cx| {
                            cx.update_global::<ClientBook, _>(|clients, _| clients.mailing = None);
                            this.sheet = None;
                            cx.notify();
                        })))
                    }),
            )
            .into_any_element()
    }
}

/// How a text area in Clients draws: the theme's size, and the theme's ink.
/// Without the ink it takes the window's default colour, which is black
/// whatever the theme — black on the dark theme's sheet.
pub(crate) fn area_style(theme: &Theme) -> crate::text_area::AreaStyle {
    crate::text_area::AreaStyle { font_size: Some(theme.text_size), ink: Some(theme.text), ..Default::default() }
}

fn caption(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div().text_size(theme.small_text_size).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ana() -> Recipient {
        Recipient {
            name: Some("Ana Ruiz".into()),
            addr: "ana@acme.com".into(),
            company: "Acme".into(),
            included: true,
            why_not: None,
        }
    }

    fn nameless() -> Recipient {
        Recipient { name: None, addr: "den@side.example".into(), company: String::new(), included: true, why_not: None }
    }

    #[test]
    fn fields_are_filled_in_for_each_person() {
        let template = "Hi {first_name}, how is {company}? I have {name} <{email}> down.";
        assert_eq!(fill(template, &ana()), "Hi Ana, how is Acme? I have Ana Ruiz <ana@acme.com> down.");
    }

    #[test]
    fn a_missing_field_takes_its_fallback_or_nothing() {
        assert_eq!(fill("Hi {first_name|there},", &nameless()), "Hi there,");
        assert_eq!(fill("Hi {first_name},", &nameless()), "Hi ,");
        assert_eq!(fill("At {company|your company}", &nameless()), "At your company");
        // Somebody with a name ignores the fallback.
        assert_eq!(fill("Hi {first_name|there},", &ana()), "Hi Ana,");
    }

    #[test]
    fn braces_that_are_not_a_field_are_left_alone() {
        assert_eq!(fill("{ \"json\": 1 } and {unknown} and {first_name", &ana()), "{ \"json\": 1 } and {unknown} and {first_name");
        assert_eq!(fill("{{first_name}}", &ana()), "{Ana}");
        // Field names are forgiving about case and spacing.
        assert_eq!(fill("{ First_Name }", &ana()), "Ana");
    }

    #[test]
    fn a_gap_is_a_field_with_nothing_to_fill_it_and_no_fallback() {
        assert!(leaves_a_gap("Hi {first_name},", &nameless()));
        assert!(!leaves_a_gap("Hi {first_name|there},", &nameless()));
        assert!(!leaves_a_gap("Hi {first_name},", &ana()));
        assert!(!leaves_a_gap("Hi {nonsense},", &nameless()), "not a field, so not a gap");
    }

    #[test]
    fn a_first_name_is_the_first_word_or_what_follows_the_comma() {
        assert_eq!(first_name("Ana Ruiz").as_deref(), Some("Ana"));
        assert_eq!(first_name("Ruiz, Ana").as_deref(), Some("Ana"));
        assert_eq!(first_name("'Ana' Ruiz").as_deref(), Some("Ana"));
        assert_eq!(first_name("   ").as_deref(), None);
    }

    fn entry(name: &str, addr: Option<&str>, automated: bool) -> Entry {
        let people = match addr {
            Some(addr) => vec![spool_store::people::Correspondent {
                id: 1,
                addr: addr.into(),
                name: name.into(),
                messages: 1,
                received: 1,
                sent: 1,
                first: 0,
                last: 0,
                last_inbound: None,
                last_outbound: None,
                accounts: Vec::new(),
            }],
            None => Vec::new(),
        };
        let mut entries = crate::clients::everyone(&people, &Default::default(), &HashSet::new(), &Default::default());
        let mut entry = entries.pop().expect("one person");
        entry.automated = automated;
        entry
    }

    #[test]
    fn everyone_is_listed_once_and_the_automated_start_unticked() {
        let entries = [
            entry("Ana Ruiz", Some("ana@acme.com"), false),
            entry("Ana again", Some("ANA@acme.com"), false),
            entry("", Some("den@side.example"), false),
            entry("Receipts", Some("noreply@shop.com"), true),
        ];
        let found = recipients(&entries);
        let addrs: Vec<&str> = found.iter().map(|r| r.addr.as_str()).collect();
        assert_eq!(addrs, ["ana@acme.com", "den@side.example", "noreply@shop.com"]);

        assert_eq!(found[0].name.as_deref(), Some("Ana Ruiz"));
        assert_eq!(found[1].name, None, "the front of an address is not a name");
        assert!(found[1].included);
        assert!(!found[2].included);
        assert_eq!(found[2].why_not, Some("Looks automated"));
    }

    #[test]
    fn what_is_filled_into_html_is_escaped_and_a_fallback_is_not() {
        let sons = Recipient { name: Some("Bo <Chen>".into()), company: "Ruiz & Sons".into(), ..ana() };
        assert_eq!(
            fill_html("<p>Hi {first_name}, at {company}</p>", &sons),
            "<p>Hi Bo, at Ruiz &amp; Sons</p>",
            "the first name is the first word; the company is escaped"
        );
        assert_eq!(fill_html("<p>{name}</p>", &sons), "<p>Bo &lt;Chen&gt;</p>");
        // Typed into the page, so HTML already.
        assert_eq!(fill_html("Hi {first_name|my&nbsp;friend},", &nameless()), "Hi my&nbsp;friend,");
    }

    #[test]
    fn an_html_copy_carries_the_formatting_and_the_same_words_as_text() {
        // As the composer hands it over: signed already, in both.
        let job = Job {
            from: Address::new("Wes", "wes@studio.example"),
            subject: "For {company}".into(),
            body: String::new(),
            html: Some(
                "<div id=\"spool-writing\"><p>Hi <b>{first_name}</b>,</p></div>\
                 <div class=\"spool-signature\">-- <br>\nWes<br>\nJoe Designs</div>"
                    .into(),
            ),
            attachments: vec![spool_core::draft::Attached::new("terms.pdf", b"%PDF".to_vec())],
            recipients: vec![ana()],
        };
        let copy = copy_for(&job, &ana());
        assert_eq!(copy.format, Format::Html);
        assert_eq!(copy.subject, "For Acme");
        let html = copy.html.expect("an HTML part");
        assert!(html.contains("<b>Ana</b>"), "{html}");
        assert!(html.contains("Joe Designs"), "the signature goes as written: {html}");
        assert!(copy.body.starts_with("Hi Ana,"), "the text version says the same: {}", copy.body);
        assert!(copy.body.contains("Joe Designs"), "{}", copy.body);
        assert_eq!(copy.attachments.len(), 1, "each copy carries what was attached");

        // The same job as text is what it always was.
        let plain = Job { html: None, body: "Hi {first_name},\n\n-- \nWes".into(), ..job };
        let copy = copy_for(&plain, &ana());
        assert_eq!(copy.format, Format::Plain);
        assert!(copy.html.is_none());
        assert_eq!(copy.body, "Hi Ana,\n\n-- \nWes");
    }
}
