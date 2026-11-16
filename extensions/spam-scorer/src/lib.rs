//! Spam scoring and classification for Spool, decided by Jev.
//!
//! The shape of this extension is set by one sentence in the contract:
//! `decorate` must be a pure function of its argument, and it has the shortest
//! deadline of any call because a list row is waiting on it. A network round
//! trip cannot happen there. So the work is split in two — mail is scored when
//! it lands or when you ask, the verdict goes in the extension's own key/value
//! store, and drawing a badge is a lookup.
//!
//! What it never does is move your mail. `modify-mail` is declared for exactly
//! one command, which files the selection and which you run by name. A
//! classifier that files on a threshold it picked is one you find out about by
//! missing something, and this host has no undo stack yet to find it with.
//!
//! **What leaves the machine.** One HTTPS request per scored message, to
//! `api.typesafe.ai` and nowhere else — the manifest grants that host and the
//! host resolves the URL before the request goes out. It carries the sender,
//! the subject, the reply-to, a count of recipients, and the first two thousand
//! characters of the plain-text body. Set `body = "none"` in `config.toml` and
//! the body stays here; the headers and the preview still go, because a
//! classifier told nothing classifies nothing.

mod jev;
mod labels;
mod verdict;

use std::sync::OnceLock;

use spool_extension::{
    Badge, CommandContext, Decorations, Effect, Emphasis, Envelope, Extension, Filter, MailboxRole,
    MoveTo, Query, Scope, export_extension, host, ui,
};

use jev::{Reading, Subject, TOP};
use verdict::{Tally, Verdict};

/// The most messages scored in one call.
///
/// A batch gets thirty seconds and a command gets thirty seconds, and each
/// message is a round trip to somebody else's API. Two dozen is comfortably
/// inside the budget on a slow day, and what is left over is not lost — an
/// unscored message stays unscored until the sweep command reaches it, which
/// is a better outcome than a call that gets interrupted halfway with nothing
/// written down.
const PER_CALL: usize = 24;

/// Failures in a row before a call stops asking. See [`score_all`].
const GIVE_UP_AFTER: usize = 3;

/// The mark on a message that was scored and had nothing wrong with it.
///
/// A plain check: it passed the scoring. It means only that the score came in
/// under the threshold, not that the message is safe — the model was asked how
/// unwanted a message is, not whether it is trustworthy. Muted, and one
/// character so the host draws it bare, because it goes on most rows.
const SCANNED: &str = "\u{2713}";

/// How much of a body the model is shown.
///
/// Spam is decided in the first screenful. Sending a whole quoted thread would
/// cost tokens to tell the model the same thing several times, and the part
/// that classifies a message is the part at the top.
const BODY_CHARS: usize = 2_000;

struct SpamScorer;

impl Extension for SpamScorer {
    fn init() -> Result<(), String> {
        // The host refuses to start an extension whose required settings are
        // missing, so a key is here by the time this runs. This checks anyway,
        // because "required" is a promise made by a file somebody edits.
        let settings = settings();
        if settings.api_key.trim().is_empty() {
            return Err("`api-key` is empty in config.toml".into());
        }

        host::log(
            host::Level::Info,
            &format!(
                "spam-scorer ready: {}, threshold {:.1}/{TOP:.0}, body {}",
                settings.model,
                settings.threshold,
                match settings.send_body {
                    true => "excerpt",
                    false => "none",
                }
            ),
        );
        Ok(())
    }

    /// New mail: score what is worth scoring, and say what happened once.
    fn on_messages(messages: Vec<Envelope>) -> Vec<Effect> {
        let settings = settings();
        let skip = uninteresting_mailboxes();

        let worth_asking: Vec<&Envelope> = messages
            .iter()
            .filter(|m| !skip.contains(&m.mailbox))
            .filter(|m| verdict::current(m.id, &settings.fingerprint).is_none())
            .collect();

        let run = score_all(&worth_asking, settings);

        let left = worth_asking.len().saturating_sub(run.scored + run.known + run.failed);
        if left > 0 {
            host::log(
                host::Level::Info,
                &format!("{left} message(s) left for a sweep — {PER_CALL} is one call's worth"),
            );
        }

        // One line for the batch. A sync that pulls four hundred messages must
        // not be four hundred anything — and a sync that pulled four hundred
        // copies of mail already scored should say nothing at all.
        match (run.scored, run.over, run.failed) {
            (0, _, 0) => Vec::new(),
            (0, _, failed) => vec![Effect::Status(format!("Spam scoring failed on {failed}"))],
            (n, 0, _) => vec![Effect::Status(format!("Scored {n} — nothing unwanted"))],
            (n, bad, _) => vec![Effect::Status(format!("Scored {n} — {bad} look unwanted"))],
        }
    }

    /// The badge on a row and the banner in the reader.
    ///
    /// A lookup and nothing else. Pure, as the contract requires, and quiet by
    /// default: an extension that badged every message would have turned the
    /// list into its own readout.
    ///
    /// A message scored *after* this was last asked keeps whatever the host
    /// cached until something invalidates it. There is no effect for that yet —
    /// `invalidate-panel` has no counterpart for decorations — so in practice
    /// the first draw after a sweep may be a frame behind the verdict.
    fn decorate(message: Envelope) -> Decorations {
        let Some(verdict) = verdict::load(message.id) else {
            return Decorations { banners: Vec::new(), badges: Vec::new() };
        };
        let settings = settings();
        let threshold = settings.threshold;

        let mut badges = Vec::new();
        let mut banners = Vec::new();
        // One of the labels that show, when Jev is sure enough that is what
        // this message is. The gate is the category's own confidence, not
        // the score's: see `Verdict::classified`.
        let label = labels::shown(&settings.labels, &verdict.c).filter(|_| verdict.classified());

        if verdict.impersonates() {
            badges.push(ui::badge("Forged", Emphasis::Danger));
            banners.push(ui::banner_with(
                format!(
                    "Jev reads this as impersonation ({:.0}% sure). Check who actually sent it \
                     before following anything in it.",
                    verdict.i * 100.0
                ),
                Emphasis::Danger,
                "File as spam",
                "file",
            ));
        } else if verdict.firm() && verdict.s >= threshold {
            badges.push(ui::badge(format!("Spam {:.1}", verdict.s), Emphasis::Danger));
            banners.push(ui::banner_with(
                format!("Scored {:.1} of {TOP:.0} for how unwanted it is.", verdict.s),
                Emphasis::Danger,
                "File as spam",
                "file",
            ));
        } else if let Some(label) = label {
            badges.push(ui::badge(label.name.clone(), Emphasis::Muted));
        } else {
            // Looked at, and passed: none of the labels that show, or not
            // sure enough to say which.
            //
            // Worth a mark of its own, because without one this row and a row
            // nothing has ever looked at are the same blank space — and most
            // rows are this one. An extension whose correct answer is silence
            // is an extension that reads as broken.
            //
            // A single character, so the host draws it bare rather than in a
            // box, and muted, so a column of them is texture rather than
            // information.
            badges.push(ui::badge(SCANNED, Emphasis::Muted));
        }

        // What Jev thinks this is, on every scored message, beside the
        // subject. Filing is offered there where a label is shown and nothing
        // above has offered it already.
        let kinds = ui::tags(kinds(&verdict));
        match label.is_some() && banners.is_empty() {
            true => banners.push(ui::with_action(kinds, "File as spam", "file")),
            false if !kinds.badges.is_empty() => banners.push(kinds),
            false => {}
        }

        Decorations { banners, badges }
    }

    fn run_command(id: String, context: CommandContext) -> Vec<Effect> {
        match id.as_str() {
            "score" => score_selection(&context),
            "sweep" => sweep(&context),
            "file" => file_as_spam(&context),
            other => {
                host::log(host::Level::Warn, &format!("no command called `{other}`"));
                Vec::new()
            }
        }
    }
}

/// How likely a category has to be to be named on the open message.
///
/// Under a coin flip, so a message Jev reads as fairly evenly two things is
/// shown as both; above a third, so no more than two ever are.
const LIKELY: f64 = 0.4;

/// What Jev thinks the message is, as badges beside the subject of the
/// open message: every category at least [`LIKELY`], or failing that its one
/// answer. `Other` is left out — "none of these" is not a kind of message.
fn kinds(verdict: &Verdict) -> Vec<Badge> {
    let mut names: Vec<&str> =
        verdict.p.iter().filter(|(_, p)| *p >= LIKELY).map(|(name, _)| name.as_str()).collect();
    if names.is_empty() {
        names.push(verdict.c.as_str());
    }
    names
        .into_iter()
        .filter(|name| *name != labels::OTHER)
        .map(|name| ui::badge(name, Emphasis::Muted))
        .collect()
}

// --- the commands -------------------------------------------------------

fn score_selection(context: &CommandContext) -> Vec<Effect> {
    if context.selection.is_empty() {
        return vec![Effect::Status("Nothing selected to score".into())];
    }

    // Selection gives ids, and scoring needs an envelope. The host has no call
    // that turns one into the other, so the open mailbox is queried and
    // narrowed — which is also why this is capped by what a query returns.
    let wanted: Vec<Envelope> = nearby(context)
        .into_iter()
        .filter(|m| context.selection.contains(&m.id))
        .collect();
    // Anything already decided about is skipped inside `score_all` rather than
    // filtered here, so the count of what was skipped survives to the status
    // line instead of vanishing into a shorter list.

    if wanted.is_empty() {
        return vec![Effect::Status("Could not find the selected messages".into())];
    }

    let borrowed: Vec<&Envelope> = wanted.iter().collect();
    let run = score_all(&borrowed, settings());

    // What was already known is said out loud rather than quietly skipped.
    // Selecting a hundred messages and being told "scored 4" reads as a
    // failure; "scored 4, 96 already done" reads as the answer.
    let mut line = match (run.scored, run.failed) {
        (0, 0) if run.known > 0 => format!("All {} already scored", run.known),
        (0, 0) => "Nothing to score".to_string(),
        (0, n) => format!("Scoring failed on {n}"),
        (n, _) => format!("Scored {n} — {} unwanted", run.over),
    };
    if run.known > 0 && run.scored > 0 {
        line.push_str(&format!(", {} already done", run.known));
    }
    if run.failed > 0 && run.scored > 0 {
        line.push_str(&format!(", {} failed", run.failed));
    }
    vec![Effect::Status(line)]
}

fn sweep(context: &CommandContext) -> Vec<Effect> {
    let skip = uninteresting_mailboxes();
    let unscored: Vec<Envelope> = nearby(context)
        .into_iter()
        .filter(|m| !skip.contains(&m.mailbox))
        .filter(|m| verdict::current(m.id, &settings().fingerprint).is_none())
        .collect();

    if unscored.is_empty() {
        return vec![Effect::Status("Everything here has been scored".into())];
    }

    let borrowed: Vec<&Envelope> = unscored.iter().collect();
    let run = score_all(&borrowed, settings());
    let left = unscored.len().saturating_sub(run.scored + run.known + run.failed);

    let mut line = format!("Scored {} — {} unwanted", run.scored, run.over);
    if run.known > 0 {
        line.push_str(&format!(", {} already known", run.known));
    }
    if left > 0 {
        line.push_str(&format!(", {left} still to do"));
    }
    if run.failed > 0 {
        line.push_str(&format!(", {} failed", run.failed));
    }
    vec![Effect::Status(line)]
}

/// Files the selection in the account's junk mailbox.
///
/// The only thing here that touches mail, and it is a command with a name,
/// because that is the difference between a tool and a surprise.
fn file_as_spam(context: &CommandContext) -> Vec<Effect> {
    if context.selection.is_empty() {
        return vec![Effect::Status("Nothing selected to file".into())];
    }

    let Some(junk) = junk_mailbox(context) else {
        return vec![Effect::Status("No junk mailbox to file into".into())];
    };

    let mut effects: Vec<Effect> = context
        .selection
        .iter()
        .map(|&message| Effect::MoveTo(MoveTo { message, mailbox: junk }))
        .collect();

    effects.push(Effect::Status(format!("Filed {} as spam", context.selection.len())));
    effects
}

/// Which junk mailbox the selection belongs in.
///
/// Accounts have their own, so this follows the context the user is in rather
/// than picking one. With nothing to go on and more than one account, it
/// declines instead of guessing — filing mail into the wrong account's junk is
/// worse than not filing it.
fn junk_mailbox(context: &CommandContext) -> Option<u64> {
    let mailboxes = host::mailboxes();

    let account = match context.account {
        Some(account) => Some(account),
        None => context
            .mailbox
            .and_then(|open| mailboxes.iter().find(|m| m.id == open))
            .map(|m| m.account),
    };

    let junk: Vec<&spool_extension::Mailbox> = mailboxes
        .iter()
        .filter(|m| matches!(m.role, MailboxRole::Junk))
        .filter(|m| account.is_none_or(|a| m.account == a))
        .collect();

    match junk.as_slice() {
        [only] => Some(only.id),
        _ => None,
    }
}

// --- scoring ------------------------------------------------------------

/// What one run of scoring did.
#[derive(Debug, Default)]
struct Run {
    /// Messages a request was made for.
    scored: usize,
    /// Of those, how many landed at or above the threshold.
    over: usize,
    /// Messages already decided about — a copy in another folder, an earlier
    /// run, or the other copy earlier in this very batch.
    known: usize,
    failed: usize,
}

/// Scores a batch, skipping anything already decided about.
///
/// The skip is what makes this affordable to run twice. A verdict is keyed by
/// the message's `Message-ID`, so the second copy of an email — in an archive,
/// in "All Mail", or simply the same row asked about again — resolves to the
/// reading that already exists and costs a lookup rather than a request.
///
/// Only requests count against the cap. A selection of two hundred where a
/// hundred and ninety are already known should score the ten, not stop after
/// twenty-four lookups.
fn score_all(messages: &[&Envelope], settings: &Settings) -> Run {
    let mut tally = Tally::load();
    let mut run = Run::default();
    let mut failing = 0;

    for message in messages {
        if run.scored + run.failed >= PER_CALL {
            break;
        }
        // A bad key, a rate limit or Jev being down fails every message the
        // same way. Three in a row is enough to say so; asking about the rest
        // would only spend the deadline being told again.
        if failing >= GIVE_UP_AFTER {
            break;
        }

        // Checked here rather than only by the caller, because the caller
        // cannot see the duplicate that this loop is about to create: two
        // copies of one email both look unscored until the first is done.
        if verdict::current(message.id, &settings.fingerprint).is_some() {
            run.known += 1;
            continue;
        }

        match score_one(message, settings) {
            Ok(reading) => {
                let verdict = Verdict::from(&reading, host::now(), &settings.fingerprint);
                verdict::save(message.id, &verdict);
                tally.record(&verdict, settings.threshold);
                tally.tokens += reading.tokens;

                run.scored += 1;
                failing = 0;
                if verdict.s >= settings.threshold {
                    run.over += 1;
                }
            }
            Err(why) => {
                run.failed += 1;
                failing += 1;
                host::log(host::Level::Warn, &format!("scoring failed: {why}"));
                tally.failed(&why);
            }
        }
    }

    tally.last_run = host::now();
    tally.save();
    run
}

fn score_one(message: &Envelope, settings: &Settings) -> Result<Reading, String> {
    jev::read(&settings.api_key, &settings.model, &subject_of(message, settings), &settings.labels)
}

/// Everything the model is told, assembled in one place.
fn subject_of(message: &Envelope, settings: &Settings) -> Subject {
    let mut subject = Subject {
        from_name: message.from.name.clone(),
        from_addr: message.from.addr.clone(),
        subject: message.subject.clone(),
        snippet: message.snippet.clone(),
        has_attachments: message.has_attachments,
        // A reply to something already here is a strong signal on its own, and
        // it costs one header rather than a body.
        in_reply_to: host::header(message.id, "in-reply-to").is_some(),
        ..Subject::default()
    };

    // The body is read whatever the setting says, because the recipient count
    // and the reply-to address come out of it and neither is the body. Only
    // the text is withheld.
    if let Some(body) = host::read_body(message.id) {
        subject.to_count = body.to.len() + body.cc.len();

        // A reply-to pointing somewhere other than the sender is the oldest
        // tell there is. Sent only when it disagrees; when it matches it is
        // the sender again, which the model already has.
        if let Some(reply_to) = body.reply_to.first()
            && !reply_to.addr.eq_ignore_ascii_case(&message.from.addr)
        {
            subject.reply_to = reply_to.addr.clone();
        }

        if settings.send_body {
            subject.body = truncate(&body.plain, BODY_CHARS);
        }
    }

    subject
}

fn truncate(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// The messages around the user, for a command that was given ids.
fn nearby(context: &CommandContext) -> Vec<Envelope> {
    let scope = match context.mailbox {
        Some(mailbox) => Scope::Mailbox(mailbox),
        None => match context.account {
            Some(account) => Scope::Account(account),
            None => Scope::Everything,
        },
    };

    // The search box, if there is anything in it, so a sweep run while a
    // search is showing sweeps what is on screen rather than everything.
    let filter = match context.search.trim().is_empty() {
        true => Filter::All,
        false => Filter::Search(context.search.clone()),
    };

    host::query_envelopes(&Query { scope, filter, limit: 1_000 })
}

/// Mailboxes whose contents there is no point scoring.
///
/// Mail you sent is not spam, a draft is not mail yet, and something already in
/// junk or trash has been decided about. Every one of these is a request that
/// would have been paid for and read by nobody.
fn uninteresting_mailboxes() -> Vec<u64> {
    host::mailboxes()
        .into_iter()
        .filter(|m| {
            matches!(
                m.role,
                MailboxRole::Sent | MailboxRole::Drafts | MailboxRole::Junk | MailboxRole::Trash
            )
        })
        .map(|m| m.id)
        .collect()
}

// --- settings -----------------------------------------------------------

/// What `config.toml` said, read once.
///
/// Seeded by the host into this extension's own storage before `init` runs,
/// under keys the host owns and this extension may read but not write. Cached
/// because `decorate` reads the threshold and is called once per message: a
/// store lookup per row is not expensive, and not doing it is free.
struct Settings {
    api_key: String,
    model: String,
    threshold: f64,
    send_body: bool,
    labels: Vec<labels::Label>,
    /// See `labels::fingerprint`.
    fingerprint: String,
}

fn settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(|| Settings {
        api_key: setting("api-key").unwrap_or_default(),
        model: setting("model").unwrap_or_else(|| "jev-latest".into()),
        threshold: setting("threshold")
            .and_then(|t| t.trim().parse::<f64>().ok())
            .unwrap_or(3.0)
            .clamp(0.0, TOP),
        // Anything but the literal `none` sends the excerpt, so a typo errs
        // towards the documented default rather than silently classifying on
        // headers alone and scoring everything a shrug.
        send_body: setting("body").map(|b| b.trim() != "none").unwrap_or(true),
        labels: labels_setting(),
        fingerprint: labels::fingerprint(&labels_setting()),
    })
}

/// One setting, from the namespace the host seeds.
fn labels_setting() -> Vec<labels::Label> {
    setting("labels").map(|json| labels::parse(&json)).unwrap_or_default()
}

fn setting(key: &str) -> Option<String> {
    host::store_get(&format!("config.{key}")).filter(|v| !v.is_empty())
}

export_extension!(SpamScorer);
