//! Mail you sent that nobody answered, brought back with a follow-up ready.
//!
//! Every message sent while reminders are on is watched (see
//! `spool_store::follow_up`). Once its wait is over with no answer, the AI is
//! asked whether it expected one — a thank-you did not — and when it did,
//! writes a short nudge. What is due shows in a strip above the message list,
//! in the inbox and All Mail, each with Nudge, which opens that follow-up as a
//! reply to everyone it went to, and Done. Nothing is sent without you.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{AnyElement, Context, SharedString, Task, div, prelude::*};
use spool_core::follow_up::{FollowUp, Waiting, follow_up_instructions, follow_up_prompt, read_follow_up};
use spool_core::{AccountId, Address, MailboxRole};
use spool_store::follow_up::Reminder;
use spool_store::{Scope, Store};

use crate::theme::Theme;
use crate::workspace::Workspace;

const DAY: i64 = 86_400;

/// How often reminders are looked at with nothing else prompting it: a wait
/// ends with the clock, not with a sync.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// The most reminders asked about in one run.
const MAX_PER_RUN: usize = 10;

/// Shown before the rest fold away under "N more".
const SHOWN: usize = 3;

/// What Nudge and Done take of a row, with the gaps around them: the room the
/// text has to leave.
const BUTTONS: f32 = 112.;

/// How long a message about to be sent waits for an answer, as the composer
/// set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    pub days: u32,
    /// Chosen by hand rather than left at the default, so it comes back
    /// without the AI being asked whether it wanted an answer.
    pub asked: bool,
}

/// The reminders' state, on the workspace.
#[derive(Default)]
pub struct FollowUps {
    /// What is due in the mailbox in front of you, longest waiting first.
    pub due: Vec<Reminder>,
    /// Whether the strip shows all of them rather than the first few.
    pub expanded: bool,
    /// Why the last check stopped short, if it did.
    pub problem: Option<String>,
    running: bool,
    again: bool,
    ticker: Option<Task<()>>,
}

impl Workspace {
    /// Starts the clock that brings reminders back when their wait is over,
    /// looking once straight away.
    pub(crate) fn start_follow_ups(&mut self, cx: &mut Context<Self>) {
        self.follow_ups.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.check_follow_ups(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(EVERY).await;
            }
        }));
    }

    /// Starts watching a message that was just sent, and stops watching what
    /// it answered or followed up on.
    pub(crate) fn remember_sent(
        &mut self,
        message_id: String,
        draft: &spool_core::draft::Draft,
        wait: Option<Wait>,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.follow_ups {
            return;
        }
        let account = self.accounts.iter().find(|a| a.email.eq_ignore_ascii_case(&draft.from.addr)).map(|a| a.id);
        let reminder = wait.zip(account).map(|(wait, account)| {
            let sent_at = now();
            Reminder {
                message_id,
                account,
                subject: draft.subject.trim().to_string(),
                sent_to: named(&draft.to, &draft.cc),
                sent_at,
                due_at: sent_at + i64::from(wait.days) * DAY,
                asked: wait.asked,
                due: false,
                nudge: String::new(),
            }
        });

        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    if let Some(reminder) = reminder {
                        store.remind(&reminder)?;
                    }
                    store.settle_reminders()
                })
                .await;
            if let Err(e) = done {
                log::warn!("watching a sent message for an answer: {e:#}");
            }
            this.update(cx, |this, cx| this.load_follow_ups(cx)).ok();
        })
        .detach();
    }

    /// Stops watching what has been answered, and looks at whatever has
    /// waited long enough. Asked again while it runs, it runs once more after.
    pub(crate) fn check_follow_ups(&mut self, cx: &mut Context<Self>) {
        if !self.settings.follow_ups {
            return;
        }
        if self.follow_ups.running {
            self.follow_ups.again = true;
            return;
        }
        self.follow_ups.running = true;

        let store = self.store.clone();
        let ready = cx.global::<crate::assist::Assistant>().is_ready();
        let authors: Vec<(AccountId, String)> = self
            .accounts
            .iter()
            .map(|a| (a.id, a.name.split_whitespace().next().unwrap_or_default().to_string()))
            .collect();

        cx.spawn(async move |this, cx| {
            let problem = look_at_reminders(&store, ready, &authors, cx).await;
            this.update(cx, |this, cx| {
                this.follow_ups.running = false;
                this.follow_ups.problem = problem;
                this.load_follow_ups(cx);
                if std::mem::take(&mut this.follow_ups.again) {
                    this.check_follow_ups(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads what is due for the mailbox in front of you.
    pub(crate) fn load_follow_ups(&mut self, cx: &mut Context<Self>) {
        let wanted = self.settings.follow_ups.then(|| self.follow_up_accounts()).flatten();
        let Some(account) = wanted else {
            if !self.follow_ups.due.is_empty() {
                self.follow_ups.due.clear();
                cx.notify();
            }
            return;
        };

        let scope = self.scope;
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let due = cx.background_executor().spawn(async move { store.due_reminders(account) }).await;
            this.update(cx, |this, cx| match due {
                // Only for the mailbox still in front of you.
                Ok(due) if this.scope == scope && this.follow_ups.due != due => {
                    this.follow_ups.due = due;
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => log::warn!("reading follow-ups: {e:#}"),
            })
            .ok();
        })
        .detach();
    }

    /// Whose reminders the mailbox in front of you shows: every account's in
    /// All Mail, one account's in its inbox, and none anywhere else — a
    /// follow-up belongs where mail that needs you does, not in Sent or Trash.
    fn follow_up_accounts(&self) -> Option<Option<AccountId>> {
        match self.scope {
            Scope::Everything => Some(None),
            Scope::Account(account) => Some(Some(account)),
            Scope::Mailbox(id) => self
                .mailboxes
                .iter()
                .find(|m| m.id == id && m.role == MailboxRole::Inbox)
                .map(|m| Some(m.account_id)),
        }
    }

    /// Turns the reminders on or off, from Settings.
    pub(crate) fn set_follow_ups(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.follow_ups = on;
        self.persist_settings(cx);
        match on {
            true => self.check_follow_ups(cx),
            false => {
                self.follow_ups.due.clear();
                self.follow_ups.problem = None;
            }
        }
        cx.notify();
    }

    /// Opens the follow-up for a reminder as a reply to everyone the message
    /// went to, with the nudge written in. The reminder stays until that is
    /// sent — the sent reply is what settles it — or it is marked done.
    pub(crate) fn nudge(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let Some(reminder) = self.follow_ups.due.iter().find(|r| r.message_id == message_id).cloned() else {
            return;
        };
        let store = self.store.clone();
        let wanted = reminder.message_id.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let Some(id) = store.sent_copy(&wanted)? else { return anyhow::Ok(None) };
                    Ok(store.envelope(id)?.zip(store.body(id)?))
                })
                .await;
            this.update(cx, |this, cx| match found {
                Ok(Some((envelope, body))) => this.follow_up_on(envelope, body, reminder.nudge.clone(), cx),
                Ok(None) => this.say(
                    format!(
                        "The sent copy of \u{201c}{}\u{201d} is not here to answer. Sync Sent and try again.",
                        reminder.subject
                    ),
                    cx,
                ),
                Err(e) => this.say(format!("Could not open the follow-up: {e:#}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Stops watching a sent message: the person has it in hand.
    pub(crate) fn dismiss_follow_up(&mut self, message_id: &str, cx: &mut Context<Self>) {
        self.follow_ups.due.retain(|r| r.message_id != message_id);
        let store = self.store.clone();
        let message_id = message_id.to_string();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = store.drop_reminder(&message_id) {
                    log::warn!("dismissing a follow-up: {e:#}");
                }
            })
            .detach();
        cx.notify();
    }

    /// The strip above the list: sent mail still waiting for an answer.
    pub(crate) fn render_follow_ups(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let due = &self.follow_ups.due;
        if due.is_empty() {
            return None;
        }
        let now = now();
        let shown = if self.follow_ups.expanded { due.len() } else { due.len().min(SHOWN) };
        // A width fixed before the text is measured, as the list's rows have:
        // gpui only ellipsizes text whose width is definite when it is
        // measured, and one left to flex is cut at a guess instead.
        let text_width = (self.list_width() - 1. - 24. - BUTTONS).max(40.);
        let hidden = due.len() - shown;

        let rows: Vec<AnyElement> = due
            .iter()
            .take(shown)
            .enumerate()
            .map(|(ix, reminder)| {
                let (nudge_id, done_id) = (reminder.message_id.clone(), reminder.message_id.clone());
                let subject = match reminder.subject.as_str() {
                    "" => "(no subject)".to_string(),
                    subject => subject.to_string(),
                };
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .w(gpui::px(text_width))
                            .flex_none()
                            .overflow_hidden()
                            .text_ellipsis()
                            .line_clamp(1)
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(
                                gpui::StyledText::new(SharedString::from(format!(
                                    "{} \u{00b7} {subject} \u{00b7} {}",
                                    reminder.sent_to,
                                    waited(now - reminder.sent_at)
                                )))
                                .with_highlights([(
                                    0..reminder.sent_to.len(),
                                    gpui::HighlightStyle { color: Some(theme.text), ..Default::default() },
                                )]),
                            ),
                    )
                    .child(
                        div()
                            .id(("follow-up-nudge", ix))
                            .flex_shrink_0()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(theme.small_text_size)
                            .text_color(theme.accent)
                            .hover(|s| s.bg(theme.hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.nudge(&nudge_id, cx)))
                            .child("Nudge"),
                    )
                    .child(
                        div()
                            .id(("follow-up-done", ix))
                            .flex_shrink_0()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .hover(|s| s.bg(theme.hover).text_color(theme.text_muted))
                            .on_click(cx.listener(move |this, _, _, cx| this.dismiss_follow_up(&done_id, cx)))
                            .child("Done"),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_muted)
                        .child(match due.len() {
                            1 => "\u{21a9} Waiting on a reply".to_string(),
                            n => format!("\u{21a9} Waiting on {n} replies"),
                        }),
                )
                .children(rows)
                .when(hidden > 0 || self.follow_ups.expanded && due.len() > SHOWN, |strip| {
                    strip.child(
                        div()
                            .id("follow-up-more")
                            .cursor_pointer()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .hover(|s| s.text_color(theme.text_muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.follow_ups.expanded = !this.follow_ups.expanded;
                                cx.notify();
                            }))
                            .child(match hidden {
                                0 => "Show fewer".to_string(),
                                n => format!("{n} more"),
                            }),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Clears what has been answered, then looks at what has waited long enough:
/// brought back with a nudge when it wanted an answer, let go when it did not.
/// Returns why it stopped, if it stopped short.
async fn look_at_reminders(
    store: &Store,
    ready: bool,
    authors: &[(AccountId, String)],
    cx: &mut gpui::AsyncApp,
) -> Option<String> {
    let store_now = store.clone();
    let checked = cx
        .background_executor()
        .spawn(async move {
            store_now.settle_reminders()?;
            store_now.reminders_to_check(now(), MAX_PER_RUN)
        })
        .await;
    let reminders = match checked {
        Ok(reminders) => reminders,
        Err(e) => return Some(format!("{e:#}")),
    };

    for reminder in reminders {
        let store_now = store.clone();
        let author = authors.iter().find(|(id, _)| *id == reminder.account).map(|(_, n)| n.clone()).unwrap_or_default();
        let looked = cx
            .background_executor()
            .spawn(async move {
                if !ready {
                    // One asked for by hand comes back as it is. The rest
                    // wait for an AI to say whether they wanted an answer.
                    if reminder.asked {
                        store_now.reminder_due(&reminder.message_id, "")?;
                    }
                    return anyhow::Ok(());
                }
                let waiting = waiting_for(&store_now, &reminder)?;
                let reply = spool_net::ai::respond(
                    &follow_up_instructions(&author, !reminder.asked),
                    &follow_up_prompt(&waiting),
                )
                .await?;
                match read_follow_up(&reply) {
                    Some(FollowUp { wanted: false, .. }) if !reminder.asked => {
                        store_now.drop_reminder(&reminder.message_id)?
                    }
                    Some(follow_up) => store_now.reminder_due(&reminder.message_id, &follow_up.nudge)?,
                    // Neither a yes nor a no: brought back rather than kept
                    // from you.
                    None => store_now.reminder_due(&reminder.message_id, "")?,
                }
                Ok(())
            })
            .await;
        if let Err(e) = looked {
            log::warn!("checking a follow-up: {e:#}");
            return Some(format!("{e:#}"));
        }
    }
    None
}

/// A reminder as the model is shown it: the message as it was sent, when its
/// copy is here, and how long it has waited.
fn waiting_for(store: &Store, reminder: &Reminder) -> anyhow::Result<Waiting> {
    let text = match store.sent_copy(&reminder.message_id)? {
        Some(id) => crate::compose::body_text(&store.body(id)?.unwrap_or_default()),
        None => String::new(),
    };
    Ok(Waiting {
        to: reminder.sent_to.clone(),
        subject: reminder.subject.clone(),
        text,
        days: ((now() - reminder.sent_at) / DAY).max(1) as u32,
    })
}

/// Who a message went to, as a reader would say it: `Ana Ruiz`,
/// `Ana Ruiz and Bo Chen`, `Ana Ruiz and 3 others`.
fn named(to: &[Address], cc: &[Address]) -> String {
    let everyone: Vec<&Address> = to.iter().chain(cc).collect();
    let name = |a: &Address| match a.name.trim() {
        "" => a.addr.trim().to_string(),
        name => name.to_string(),
    };
    match everyone.as_slice() {
        [] => "Nobody".to_string(),
        [one] => name(one),
        [one, two] => format!("{} and {}", name(one), name(two)),
        [one, rest @ ..] => format!("{} and {} others", name(one), rest.len()),
    }
}

/// How long something has waited, in the strip's words.
fn waited(seconds: i64) -> String {
    match (seconds / DAY).max(1) {
        1 => "1 day".to_string(),
        days if days < 14 => format!("{days} days"),
        days => format!("{} weeks", days / 7),
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, addr: &str) -> Address {
        Address::new(name, addr)
    }

    #[test]
    fn who_it_went_to_is_named_the_way_a_reader_would() {
        let ana = person("Ana Ruiz", "ana@example.com");
        let bo = person("", "bo@example.com");
        let cy = person("Cy", "cy@example.com");
        assert_eq!(named(std::slice::from_ref(&ana), &[]), "Ana Ruiz");
        assert_eq!(named(std::slice::from_ref(&ana), std::slice::from_ref(&bo)), "Ana Ruiz and bo@example.com");
        assert_eq!(named(&[ana, bo], &[cy]), "Ana Ruiz and 2 others");
        assert_eq!(named(&[], &[]), "Nobody");
    }

    #[test]
    fn a_wait_is_said_in_days_then_weeks() {
        assert_eq!(waited(0), "1 day");
        assert_eq!(waited(3 * DAY + 5), "3 days");
        assert_eq!(waited(21 * DAY), "3 weeks");
    }
}
