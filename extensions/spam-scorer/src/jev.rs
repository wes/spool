//! Asking Jev.
//!
//! Jev is TypeSafe's System One model: you hand it some state and a map of
//! typed questions, and it hands back one typed answer per question — a level
//! on a scale, an option from a set, a probability. It does not return prose,
//! which is the whole reason it is the right model for this job. A classifier
//! built on a chat model spends half its code being suspicious of a string, and
//! the half of it that parses is the half that breaks on the first answer that
//! begins "Sure!".
//!
//! Three questions go out per message, in one request. The API evaluates each
//! one in parallel and in isolation, so asking three costs about what asking
//! one costs, and a question that is wrong about the others cannot drag them
//! with it:
//!
//! * `spam` — a five-level [`Score`], from mail you asked for to outright
//!   fraud. Ordered, so the answer that comes back is a position on the scale
//!   rather than a bucket, and a 2.4 is a genuinely ambiguous message rather
//!   than a coin flip somebody rounded for us.
//! * `category` — a [`Choice`], for what the message *is*. Unordered, because
//!   a newsletter is not more or less of anything than a receipt. The options
//!   are the labels from the extension's settings (see `labels.rs`), plus
//!   `Other` for mail that is none of them.
//! * `impersonation` — a yes/no probability. Separate from the score because
//!   the two really are separate: plenty of forged mail scores low on bulk, and
//!   the response to "this is pretending to be your bank" is not the response
//!   to "this is marketing".
//!
//! Splitting them is not fussiness. The documented failure mode of a single
//! question that measures three things is that it measures none of them well.

use serde::Deserialize;
use serde_json::{Value, json};
use spool_extension::host;

use crate::labels::{Label, OTHER};

/// The one host this extension may reach, and the only place a key goes.
pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// The top of the spam scale — the number a raw score is out of.
pub const TOP: f64 = 4.0;

/// What the model was asked and what it said.
#[derive(Debug, Clone)]
pub struct Reading {
    /// 0–4, probability-weighted across the levels, so it lands between them.
    pub spam: f64,
    /// How concentrated the spam answer was. 1.0 is a single level; a reading
    /// spread over three is worth acting on less readily.
    pub spam_confidence: f64,
    pub category: String,
    pub category_confidence: f64,
    /// The likeliest categories and how likely each is, most likely first.
    pub top: Vec<(String, f64)>,
    /// The probability that the sender is pretending to be someone else.
    pub impersonation: f64,
    /// Input tokens, for the panel's running total. Output is not billed —
    /// there is no generated text to bill for.
    pub tokens: u64,
}

/// Everything the model is told about a message.
///
/// Assembled by the caller so that what leaves the machine is visible in one
/// place rather than spread through the code that gathers it.
#[derive(Debug, Default)]
pub struct Subject {
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub snippet: String,
    /// The plain-text body, already truncated. Empty when the user has asked
    /// for headers only.
    pub body: String,
    /// The `Reply-To`, when there is one and it differs from the sender.
    pub reply_to: String,
    pub to_count: usize,
    pub has_attachments: bool,
    /// Whether this message is a reply to something in the mailbox already.
    pub in_reply_to: bool,
}

impl Subject {
    fn state(&self) -> Value {
        json!({
            "from_name": self.from_name,
            "from_address": self.from_addr,
            "reply_to": self.reply_to,
            "subject": self.subject,
            "preview": self.snippet,
            "body": self.body,
            "recipient_count": self.to_count,
            "has_attachments": self.has_attachments,
            "is_a_reply": self.in_reply_to,
        })
    }
}

/// How many categories a reading keeps, for the detail line.
const TOP_CATEGORIES: usize = 3;

/// Asks the three questions and reads the three answers.
pub fn read(key: &str, model: &str, subject: &Subject, labels: &[Label]) -> Result<Reading, String> {
    let mut questions = serde_json::Map::new();
    questions.insert("spam".into(), spam_question());
    // With every label removed there is nothing to choose between.
    if !labels.is_empty() {
        questions.insert("category".into(), category_question(labels));
    }
    questions.insert("impersonation".into(), impersonation_question());

    let body = json!({
        "model": model,
        "state": subject.state(),
        "questions": questions,
    });

    let request = host::Request {
        method: "POST".into(),
        url: ENDPOINT.into(),
        headers: vec![
            ("authorization".into(), format!("Bearer {key}")),
            ("content-type".into(), "application/json".into()),
        ],
        body: Some(serde_json::to_vec(&body).map_err(|e| e.to_string())?),
    };

    let response = host::fetch(&request)?;

    // A failure is reported and dropped rather than retried. There is no way
    // to sleep inside the sandbox, so "retry with backoff" would be a spin
    // against a deadline — and a message that went unscored is one the sweep
    // command picks up the next time it is run, which is a better answer than
    // burning this call's budget on the same 429.
    if response.status != 200 {
        return Err(explain(response.status, &response.body));
    }

    let answers: Envelope =
        serde_json::from_slice(&response.body).map_err(|e| format!("unreadable answer: {e}"))?;

    let Some(Answer::Score { score, confidence, .. }) = answers.answers.get("spam") else {
        return Err("no spam score came back".into());
    };
    let (category, category_confidence, top) = match answers.answers.get("category") {
        Some(Answer::Choice { choice, confidence, probabilities }) => {
            let mut top: Vec<(String, f64)> =
                probabilities.iter().map(|(name, p)| (name.clone(), *p)).collect();
            top.sort_by(|a, b| b.1.total_cmp(&a.1));
            top.truncate(TOP_CATEGORIES);
            (choice.clone(), *confidence, top)
        }
        _ => (OTHER.to_string(), 0.0, Vec::new()),
    };
    let impersonation = match answers.answers.get("impersonation") {
        Some(Answer::Noul { noul }) => *noul,
        _ => 0.0,
    };

    Ok(Reading {
        spam: *score,
        spam_confidence: *confidence,
        category,
        category_confidence,
        top,
        impersonation,
        tokens: answers.usage.input_tokens,
    })
}

/// The scale.
///
/// Levels describe situations rather than degrees, which is the difference
/// between a level the model can match a message against and an adjective it
/// has to interpret. "Broken but there is a workaround" is a level; "moderately
/// severe" is a mood. They are also evaluated independently — the model does
/// not see the numbers or the neighbours — so none of them may say "more than
/// the last one".
fn spam_question() -> Value {
    json!({
        "type": "score",
        "instructions":
            "How unwanted is this message to the person who received it? Judge the \
             message itself, not whether you personally find the subject interesting.",
        "criteria": [
            "Mail this person asked for: someone writing to them directly, a reply in \
             a conversation they are part of, or a receipt, alert or account notice \
             from a service they clearly use.",
            "A newsletter, digest, or product update from a sender this person signed \
             up with. Sent to a list, wanted at the time, possibly stale now.",
            "Unsolicited but genuine: a real person or company with no prior \
             relationship to this person, writing about something real — cold sales, \
             recruiting, a conference invitation, a partnership pitch.",
            "Bulk commercial mail sent to an address that never asked for it: mass \
             marketing, a scraped or purchased list, a misleading subject line, an \
             unsubscribe link that is the point of the message.",
            "Malicious: a forged sender, a link or form harvesting credentials or \
             payment details, advance-fee or lottery fraud, extortion, or an \
             attachment that has no business being attached.",
        ]
    })
}

/// What the message is, as opposed to how unwanted it is: which label.
///
/// The names are the options and what comes back, so a label is drawn by the
/// name Jev answers with. The definitions are what the model matches against.
fn category_question(labels: &[Label]) -> Value {
    let mut criteria = serde_json::Map::new();
    for label in labels {
        let definition = match label.definition.is_empty() {
            // The docs' own advice: a name that explains itself can go with
            // no description at all.
            true => Value::Null,
            false => Value::String(label.definition.clone()),
        };
        criteria.insert(label.name.clone(), definition);
    }
    criteria.insert(OTHER.into(), Value::String("None of the above fits this message.".into()));
    json!({
        "type": "choice",
        "instructions": "What kind of message is this? Pick the single best fit.",
        "criteria": criteria,
    })
}

fn impersonation_question() -> Value {
    json!({
        "type": "noul",
        "instructions":
            "Is the sender pretending to be a person, company or service they are not?",
        "criteria": {
            "true": "The display name, the wording, or the branding claims an identity \
                     the sending address does not support — a bank, a well-known \
                     service, a colleague, an executive — or the reply address points \
                     somewhere unrelated to the claimed sender.",
            "false": "The sender is who they appear to be, including ordinary bulk mail \
                      from a company that really does send it.",
        }
    })
}

/// The status codes the API documents, said in a way that names the fix.
fn explain(status: u16, body: &[u8]) -> String {
    let detail = match std::str::from_utf8(body) {
        Ok(text) if !text.trim().is_empty() => format!(": {}", first(text, 200)),
        _ => String::new(),
    };

    match status {
        401 => "Jev refused the API key — check `api-key` in config.toml".to_string(),
        422 => format!("Jev refused the request as malformed{detail}"),
        429 => "Jev is rate limiting; run the sweep command again in a minute".to_string(),
        529 => "Jev is overloaded; try again shortly".to_string(),
        other => format!("Jev answered {other}{detail}"),
    }
}

fn first(text: &str, n: usize) -> String {
    match text.char_indices().nth(n) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    answers: std::collections::BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
}

/// One typed answer.
///
/// Tagged by `type`, exactly as the API tags it, so an answer of the wrong
/// shape for the question it was asked fails to deserialize rather than being
/// silently read as something else. Fields the API may omit default rather than
/// failing the whole response: a missing confidence is a reading we act on less
/// readily, not an outage.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Answer {
    Score {
        score: f64,
        #[serde(default)]
        confidence: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        confidence: f64,
        #[serde(default)]
        probabilities: std::collections::BTreeMap<String, f64>,
    },
    Noul {
        noul: f64,
    },
}
