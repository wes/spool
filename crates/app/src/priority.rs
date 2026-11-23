//! Priority: new mail sorted by whether it needs you, and the quick replies
//! kept ready for the mail that wants an answer.
//!
//! The sort runs after every sync, on inbox mail it has not seen, and asks the
//! AI provider chosen in Settings in batches. Mail from a mailing list, and
//! your own, is sorted without asking. Verdicts go into the store (see
//! `spool_store::triage`), and the Priority filter is a query over them, so
//! it works in any inbox, any account, or all of them at once.
//!
//! Off until it is turned on in Settings → AI, because it is the one AI
//! feature that reads mail nobody asked it to.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*};
use spool_core::priority::{
    self as sort, Addressed, BATCH, Candidate, Kind, QuickReply, quick_reply_instructions,
    quick_reply_prompt, read_quick_replies, read_triage, triage_instructions, triage_prompt,
};
use spool_core::{Account, Envelope, MessageId, unsubscribe::ListUnsubscribe};
use spool_store::triage::{SortedBy, Triage};
use spool_store::Store;

use crate::actions::{MarkPriority, QuickReply1, QuickReply2, QuickReply3, TogglePriorityFilter};
use crate::theme::Theme;
use crate::workspace::{Filter, Workspace};

/// How far back the sort reaches. Turned on for the first time, it sorts the
/// last few days rather than everything ever received: what needs you from
/// last year has either been dealt with or is not going to be.
const LOOK_BACK: i64 = 3 * 24 * 60 * 60;

/// The most messages asked about in one run. A first run over a busy inbox
/// is a few of these; the rest is sorted after the next sync.
const MAX_PER_RUN: usize = 8 * BATCH;

/// Replies written in one run, each a request of its own.
const MAX_REPLIES_PER_RUN: usize = 6;

/// How many hand-made decisions the sort is told about.
const LEARNED: usize = 40;

/// The Priority sort's state, on the workspace.
#[derive(Default)]
pub struct Priority {
    /// The open message's verdict and replies, once read.
    pub open: Option<OpenPriority>,
    /// Why the last sort stopped short, if it did.
    pub problem: Option<String>,
    running: bool,
    /// Asked for again while a run was going.
    again: bool,
}

pub struct OpenPriority {
    pub message: MessageId,
    pub triage: Option<Triage>,
    pub replies: Option<Vec<QuickReply>>,
}

/// One of your accounts, as the sort needs it.
#[derive(Clone)]
struct You {
    account: spool_core::AccountId,
    name: String,
    email: String,
}

impl Workspace {
    fn yourselves(&self) -> Vec<You> {
        self.accounts
            .iter()
            .map(|a: &Account| You { account: a.id, name: a.name.clone(), email: a.email.to_lowercase() })
            .collect()
    }

    /// Sorts the inbox mail that has not been sorted, then writes replies
    /// for what wants one. Does nothing while Priority is off or the AI is
    /// not connected; asked again while it runs, it runs once more after.
    pub fn sort_new_mail(&mut self, cx: &mut Context<Self>) {
        if !self.settings.priority || !cx.global::<crate::assist::Assistant>().is_ready() {
            return;
        }
        if self.priority.running {
            self.priority.again = true;
            return;
        }
        self.priority.running = true;

        let store = self.store.clone();
        let you = self.yourselves();
        let replies = self.settings.quick_replies;

        cx.spawn(async move |this, cx| {
            let since = now() - LOOK_BACK;
            let mut asked = 0;
            let mut problem = None;

            while asked < MAX_PER_RUN {
                let (store_now, you_now) = (store.clone(), you.clone());
                let prepared =
                    cx.background_executor().spawn(async move { prepare_batch(&store_now, &you_now, since) }).await;
                let batch = match prepared {
                    Ok(batch) => batch,
                    Err(e) => {
                        problem = Some(format!("{e:#}"));
                        break;
                    }
                };
                if batch.asking.is_empty() {
                    // Rules may have sorted a page without asking; there may
                    // be more behind it.
                    if batch.ruled == 0 {
                        break;
                    }
                    this.update(cx, |this, cx| this.priority_changed(cx)).ok();
                    continue;
                }

                let count = batch.asking.len();
                let (instructions, prompt) = (batch.instructions, batch.prompt);
                let reply = cx
                    .background_executor()
                    .spawn(async move { spool_net::ai::respond(&instructions, &prompt).await })
                    .await;
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(e) => {
                        log::warn!("sorting mail by priority: {e:#}");
                        problem = Some(format!("{e:#}"));
                        break;
                    }
                };

                let store_now = store.clone();
                let ids: Vec<MessageId> = batch.asking.iter().map(|(id, _)| *id).collect();
                let saved = cx
                    .background_executor()
                    .spawn(async move {
                        for (id, verdict) in ids.into_iter().zip(read_triage(&reply, count)) {
                            // Nothing usable about it: put it with the rest
                            // rather than asking about it after every sync.
                            let verdict = verdict.unwrap_or(sort::Verdict { kind: Kind::Other, reason: String::new() });
                            store_now.set_triage(
                                id,
                                &Triage { kind: verdict.kind, reason: verdict.reason, by: SortedBy::Model },
                            )?;
                        }
                        anyhow::Ok(())
                    })
                    .await;
                if let Err(e) = saved {
                    problem = Some(format!("{e:#}"));
                    break;
                }
                asked += count;
                log::info!("sorted {count} messages by priority");
                this.update(cx, |this, cx| this.priority_changed(cx)).ok();
            }

            if replies && problem.is_none() {
                problem = write_quick_replies(&store, &you, since, &this, cx).await;
            }

            this.update(cx, |this, cx| {
                this.priority.running = false;
                this.priority.problem = problem;
                if std::mem::take(&mut this.priority.again) {
                    this.sort_new_mail(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Something was sorted: the list and the open message may both say
    /// something different now.
    fn priority_changed(&mut self, cx: &mut Context<Self>) {
        if self.filter == Filter::Priority {
            self.reload(cx);
        }
        self.load_open_priority(cx);
    }

    /// Reads the open message's verdict and replies, for the bar above it.
    pub(crate) fn load_open_priority(&mut self, cx: &mut Context<Self>) {
        let Some(message) = self.open.as_ref().map(|open| open.envelope.id) else {
            self.priority.open = None;
            return;
        };
        if !self.settings.priority {
            self.priority.open = None;
            return;
        }
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    let triage = store.triage(message).ok().flatten();
                    let replies = store.quick_replies(message).ok().flatten();
                    OpenPriority { message, triage, replies }
                })
                .await;
            this.update(cx, |this, cx| {
                // Only for the message still open.
                if this.open.as_ref().map(|open| open.envelope.id) == Some(message) {
                    this.priority.open = Some(read);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Turns the sort on or off, from Settings.
    pub(crate) fn set_priority(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.priority = on;
        self.persist_settings(cx);
        if on {
            self.sort_new_mail(cx);
            self.load_open_priority(cx);
        } else {
            self.priority.open = None;
            if self.filter == Filter::Priority {
                self.clear_filter(cx);
            }
        }
        cx.notify();
    }

    /// Asks to turn Priority on, where something needed it.
    fn ask_for_priority(&mut self, cx: &mut Context<Self>) {
        self.say("Turn on \u{201c}Sort new mail by priority\u{201d} in Settings \u{2192} AI", cx);
        crate::assist::open_settings(&cx.entity().downgrade(), cx);
    }

    /// The `p` key and the list header's button: priority mail in what is
    /// being shown, or all of it again. All Mail with it on is every inbox's.
    pub(crate) fn toggle_priority_filter_now(&mut self, cx: &mut Context<Self>) {
        if !self.settings.priority {
            self.ask_for_priority(cx);
            return;
        }
        self.filter = match self.filter {
            Filter::Priority => Filter::All,
            _ => Filter::Priority,
        };
        self.selected = 0;
        self.chosen = None;
        self.reload(cx);
    }

    /// Moves the selected row into Priority, or out of it. Made by hand, so
    /// it stands, and the sort is told about it from then on.
    pub(crate) fn mark_priority_now(&mut self, cx: &mut Context<Self>) {
        if !self.settings.priority {
            self.ask_for_priority(cx);
            return;
        }
        let Some(envelope) = self.selected_envelope().cloned() else { return };
        let ids: Vec<MessageId> = match &envelope.group {
            Some(group) => group.ids.clone(),
            None => vec![envelope.id],
        };
        let store = self.store.clone();
        let sender = match envelope.from.name.trim() {
            "" => envelope.from.addr.clone(),
            name => name.to_string(),
        };

        cx.spawn(async move |this, cx| {
            let moved = cx
                .background_executor()
                .spawn(async move {
                    let now_priority =
                        store.triage(envelope.id)?.is_some_and(|t: Triage| t.kind.is_priority());
                    // In by hand is "needs you", with no reason of the
                    // model's to show: the list falls back to the first line.
                    let triage = match now_priority {
                        true => Triage { kind: Kind::Other, reason: String::new(), by: SortedBy::Person },
                        false => Triage { kind: Kind::Act, reason: String::new(), by: SortedBy::Person },
                    };
                    for id in &ids {
                        store.set_triage(*id, &triage)?;
                    }
                    anyhow::Ok(!now_priority)
                })
                .await;
            this.update(cx, |this, cx| match moved {
                Ok(true) => {
                    this.say(format!("Moved into Priority. Mail from {sender} will be sorted with this in mind"), cx);
                    this.priority_changed(cx);
                }
                Ok(false) => {
                    this.say(format!("Moved out of Priority. Mail from {sender} will be sorted with this in mind"), cx);
                    this.priority_changed(cx);
                }
                Err(e) => this.say(format!("Couldn\u{2019}t change Priority: {e:#}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Opens the open message's `index`th quick reply as a reply.
    fn use_quick_reply(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(open) = self.priority.open.as_ref() else { return };
        if self.open.as_ref().map(|o| o.envelope.id) != Some(open.message) {
            return;
        }
        let Some(reply) = open.replies.as_ref().and_then(|r| r.get(index)).cloned() else { return };
        self.reply_with_text(reply.text, cx);
    }

    pub(crate) fn toggle_priority_filter(&mut self, _: &TogglePriorityFilter, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_priority_filter_now(cx);
    }

    pub(crate) fn mark_priority(&mut self, _: &MarkPriority, _: &mut Window, cx: &mut Context<Self>) {
        self.mark_priority_now(cx);
    }

    pub(crate) fn quick_reply_1(&mut self, _: &QuickReply1, _: &mut Window, cx: &mut Context<Self>) {
        self.use_quick_reply(0, cx);
    }

    pub(crate) fn quick_reply_2(&mut self, _: &QuickReply2, _: &mut Window, cx: &mut Context<Self>) {
        self.use_quick_reply(1, cx);
    }

    pub(crate) fn quick_reply_3(&mut self, _: &QuickReply3, _: &mut Window, cx: &mut Context<Self>) {
        self.use_quick_reply(2, cx);
    }

    /// Above a message the sort put in Priority: what it needs, a way out,
    /// and the replies ready for it.
    pub(crate) fn render_priority_bar(&self, id: MessageId, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.priority.open.as_ref().filter(|open| open.message == id)?;
        let triage = open.triage.as_ref().filter(|t| t.kind.is_priority())?;
        let replies: Vec<QuickReply> = open.replies.clone().unwrap_or_default();

        let what = match triage.kind {
            Kind::Reply => "Needs a reply",
            _ => "Needs you",
        };
        let line: SharedString = match triage.reason.trim() {
            "" => what.into(),
            reason => format!("{what} \u{00b7} {reason}").into(),
        };

        let cards = replies.into_iter().enumerate().map(|(i, reply)| {
            let preview: String = reply.text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
            div()
                .id(("quick-reply", i))
                .flex()
                .flex_col()
                .gap_0p5()
                .flex_1()
                .min_w_0()
                .px_2p5()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .hover(|s| s.border_color(theme.accent))
                .on_click(cx.listener(move |this, _, _, cx| this.use_quick_reply(i, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_faint)
                                .child(format!("{}", i + 1)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(reply.label),
                        ),
                )
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_muted)
                        .line_clamp(2)
                        .child(preview),
                )
        });

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mx_6()
                .mt_3()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border)
                .text_size(theme.small_text_size)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_shrink_0().text_color(theme.accent).child(PRIORITY))
                        .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(line))
                        .child(
                            div()
                                .id("not-priority")
                                .flex_shrink_0()
                                .cursor_pointer()
                                .text_color(theme.text_faint)
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| this.mark_priority_now(cx)))
                                .child("Not priority"),
                        ),
                )
                .children((!open.replies.as_ref().is_none_or(Vec::is_empty)).then(|| {
                    div().flex().gap_2().children(cards)
                }))
                .into_any_element(),
        )
    }

    /// The list header's Priority button, while Priority is on.
    pub(crate) fn render_priority_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.priority {
            return None;
        }
        let on = self.filter == Filter::Priority;
        Some(
            crate::chrome::toggle_button(
                "priority-filter",
                PRIORITY.into(),
                if on { "Show all mail".into() } else { "Show priority mail".into() },
                on,
                theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_priority_filter_now(cx)))
            .into_any_element(),
        )
    }
}

/// Priority's mark: a star, from the text font rather than the emoji one,
/// so it takes the theme's colours like the flag beside it.
pub(crate) const PRIORITY: &str = "\u{2605}";

/// A page of unsorted mail: what the rules sorted on the spot, and what is
/// left to ask about.
struct Batch {
    ruled: usize,
    asking: Vec<(MessageId, Candidate)>,
    instructions: String,
    prompt: String,
}

/// Reads the next unsorted mail, sorts what the rules can, and builds the
/// request for the rest.
fn prepare_batch(store: &Store, you: &[You], since: i64) -> anyhow::Result<Batch> {
    let mut ruled = 0;
    let mut asking = Vec::new();
    // More than a batch, because rules take some without asking.
    for id in store.untriaged(since, BATCH * 4)? {
        let Some(envelope) = store.envelope(id)? else { continue };
        let body = store.body(id)?.unwrap_or_default();
        if let Some(triage) = by_rule(&envelope, &body, you) {
            store.set_triage(id, &triage)?;
            ruled += 1;
            continue;
        }
        asking.push((id, candidate(&envelope, &body, you)));
        if asking.len() == BATCH {
            break;
        }
    }

    let candidates: Vec<Candidate> = asking.iter().map(|(_, c)| c.clone()).collect();
    let learned = store.learned(LEARNED)?;
    Ok(Batch {
        ruled,
        instructions: triage_instructions(&who(you)),
        prompt: triage_prompt(&candidates, &learned),
        asking,
    })
}

/// Mail that is not worth asking about: your own, and a mailing list's.
fn by_rule(envelope: &Envelope, body: &spool_core::Body, you: &[You]) -> Option<Triage> {
    let other = Triage { kind: Kind::Other, reason: String::new(), by: SortedBy::Rule };
    let from = envelope.from.addr.to_lowercase();
    if you.iter().any(|y| y.email == from) {
        return Some(other);
    }
    if matches!(body.list_unsubscribe, ListUnsubscribe::Offered { .. }) {
        return Some(other);
    }
    None
}

fn candidate(envelope: &Envelope, body: &spool_core::Body, you: &[You]) -> Candidate {
    let mine = |list: &[spool_core::Address]| list.iter().any(|a| you.iter().any(|y| y.email.eq_ignore_ascii_case(&a.addr)));
    let addressed = if mine(&body.to) {
        Addressed::To
    } else if mine(&body.cc) {
        Addressed::Cc
    } else {
        Addressed::Neither
    };
    Candidate {
        from: sender_of(envelope),
        addressed,
        subject: envelope.subject.clone(),
        text: crate::compose::body_text(body),
    }
}

/// Who the mail is for, for the sort's instructions: a name, and every
/// address.
fn who(you: &[You]) -> String {
    let name = you.iter().map(|y| y.name.trim()).find(|n| !n.is_empty()).unwrap_or_default();
    let emails: Vec<&str> = you.iter().map(|y| y.email.as_str()).collect();
    match (name, emails.is_empty()) {
        (_, true) => name.to_string(),
        ("", false) => emails.join(", "),
        (name, false) => format!("{name} ({})", emails.join(", ")),
    }
}

fn sender_of(envelope: &Envelope) -> String {
    match envelope.from.name.trim() {
        "" => envelope.from.addr.clone(),
        name => format!("{name} <{}>", envelope.from.addr),
    }
}

/// Writes replies for priority mail that wants an answer and has none ready.
/// Returns why it stopped, if it stopped short.
async fn write_quick_replies(
    store: &Store,
    you: &[You],
    since: i64,
    this: &gpui::WeakEntity<Workspace>,
    cx: &mut gpui::AsyncApp,
) -> Option<String> {
    let store_now = store.clone();
    let wanting = cx
        .background_executor()
        .spawn(async move { store_now.wanting_quick_replies(since, MAX_REPLIES_PER_RUN) })
        .await;
    let wanting = match wanting {
        Ok(wanting) => wanting,
        Err(e) => return Some(format!("{e:#}")),
    };

    for id in wanting {
        let (store_now, you_now) = (store.clone(), you.to_vec());
        let written = cx
            .background_executor()
            .spawn(async move {
                let envelope = store_now.envelope(id)?.ok_or_else(|| anyhow::anyhow!("gone"))?;
                let body = store_now.body(id)?.unwrap_or_default();
                let author = you_now
                    .iter()
                    .find(|y| y.account == envelope.account_id)
                    .and_then(|y| y.name.split_whitespace().next())
                    .unwrap_or_default()
                    .to_string();
                let answering = spool_core::assist::Answering {
                    from: sender_of(&envelope),
                    subject: envelope.subject.clone(),
                    text: crate::compose::body_text(&body),
                };
                let reply =
                    spool_net::ai::respond(&quick_reply_instructions(&author), &quick_reply_prompt(&answering)).await?;
                let replies = read_quick_replies(&reply);
                store_now.set_quick_replies(id, &replies)?;
                anyhow::Ok(replies.len())
            })
            .await;
        match written {
            Ok(count) => {
                log::info!("wrote {count} quick replies");
                this.update(cx, |this, cx| this.load_open_priority(cx)).ok();
            }
            Err(e) => {
                log::warn!("writing quick replies: {e:#}");
                return Some(format!("{e:#}"));
            }
        }
    }
    None
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_core::{AccountId, Address};

    fn you() -> Vec<You> {
        vec![You { account: AccountId(1), name: "Sam Example".into(), email: "sam@example.com".into() }]
    }

    fn envelope(from: &str) -> Envelope {
        Envelope {
            id: MessageId(1),
            account_id: AccountId(1),
            mailbox_id: spool_core::MailboxId(1),
            thread_id: spool_core::ThreadId(1),
            from: Address::new("", from),
            subject: "Hello".into(),
            date: 0,
            snippet: String::new(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 0,
            group: None,
        }
    }

    #[test]
    fn your_own_mail_and_a_lists_are_sorted_without_asking() {
        let body = spool_core::Body::default();
        assert!(by_rule(&envelope("SAM@example.com"), &body, &you()).is_some());

        let listed = spool_core::Body {
            list_unsubscribe: ListUnsubscribe::Offered { header: "<mailto:x@example.com>".into(), one_click: false },
            ..Default::default()
        };
        let ruled = by_rule(&envelope("news@example.com"), &listed, &you()).unwrap();
        assert_eq!((ruled.kind, ruled.by), (Kind::Other, SortedBy::Rule));

        // A person writing to you is a question for the model.
        assert!(by_rule(&envelope("ana@example.com"), &body, &you()).is_none());
    }

    #[test]
    fn how_you_were_addressed_comes_from_the_recipients() {
        let mut body = spool_core::Body { to: vec![Address::new("", "Sam@Example.com")], ..Default::default() };
        assert_eq!(candidate(&envelope("ana@example.com"), &body, &you()).addressed, Addressed::To);
        body.cc = std::mem::take(&mut body.to);
        assert_eq!(candidate(&envelope("ana@example.com"), &body, &you()).addressed, Addressed::Cc);
        body.cc.clear();
        assert_eq!(candidate(&envelope("ana@example.com"), &body, &you()).addressed, Addressed::Neither);
    }

    #[test]
    fn the_sort_is_told_who_you_are() {
        assert_eq!(who(&you()), "Sam Example (sam@example.com)");
        assert_eq!(who(&[]), "");
    }
}
