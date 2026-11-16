//! What was decided about a message, and what it cost to decide it.
//!
//! Every reading is written to the extension's own key/value store and read
//! back from there. That is not an optimisation, it is the only way the
//! decoration surface can work at all: `decorate` has a fifty-millisecond
//! deadline and must be a pure function of its argument, so it can look a
//! verdict up and it cannot go and get one.
//!
//! The keys are message ids, which the contract is explicit about being local
//! handles rather than portable names. Storing one is storing a fact about this
//! machine's store, and a verdict cache that travelled to another machine would
//! be attached to whatever messages happened to hold those ids there. It does
//! not travel; nothing here writes anything that should.

use serde::{Deserialize, Serialize};
use spool_extension::host;

use crate::jev::Reading;

/// Bumped when the shape of a key or a value below changes.
///
/// In the key rather than in the value, so an old cache is not read and
/// misinterpreted — it is simply not found, and the messages are scored again.
/// The cost of a bump is one re-score of whatever had been done; the cost of
/// reading a v1 row as a v2 one is a wrong answer nobody can see is wrong.
const VERSION: &str = "v2";

/// What is kept about one message.
///
/// Short field names because every one of these is a row in a key/value store
/// that may hold a few hundred thousand of them, and `impersonation` spelled
/// out is twelve bytes per message to say nothing new.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// 0–4.
    pub s: f64,
    /// Confidence in the score.
    pub sc: f64,
    /// The category: a label's name, or `Other`.
    pub c: String,
    /// Confidence in the category.
    pub cc: f64,
    /// Probability of impersonation.
    pub i: f64,
    /// When it was decided, in unix seconds.
    pub at: i64,
    /// The likeliest categories and their probabilities, most likely first.
    #[serde(default)]
    pub p: Vec<(String, f64)>,
    /// The labels it was asked under; see `labels::fingerprint`. A verdict
    /// from before labels were settings has none, and is scored again.
    #[serde(default)]
    pub f: String,
}

impl Verdict {
    pub fn from(reading: &Reading, at: i64, fingerprint: &str) -> Self {
        Self {
            s: reading.spam,
            sc: reading.spam_confidence,
            c: reading.category.clone(),
            cc: reading.category_confidence,
            i: reading.impersonation,
            at,
            p: reading.top.clone(),
            f: fingerprint.to_string(),
        }
    }

    /// Whether this was reached under the labels as they are now.
    pub fn is_current(&self, fingerprint: &str) -> bool {
        self.f == fingerprint
    }

    /// Whether this looks like a forgery, at a probability worth saying so at.
    ///
    /// High on purpose. "This is pretending to be your bank" is the strongest
    /// thing this extension says, and a banner that says it about a genuine
    /// newsletter twice teaches people to ignore it the third time.
    pub fn impersonates(&self) -> bool {
        self.i >= 0.7
    }

    /// Whether the reading was concentrated enough to act on.
    ///
    /// Low on purpose, and it took real mail to work out why. Confidence here
    /// is how peaked the distribution over the five levels was, and on an
    /// *ordered* scale a reading split between two neighbouring levels is not
    /// the model hedging — it is the answer. A 2.35 means "between unsolicited
    /// and bulk", the score being a probability-weighted mean is what encodes
    /// that, and it comes back with a confidence near 0.45 every time.
    ///
    /// Gating those out double-counts the uncertainty the score already
    /// carries. On a real inbox a 0.5 floor threw away every reading in the
    /// 2–3 band and left two messages in twenty-eight marked at all, which
    /// reads as an extension that does not work.
    ///
    /// So this filters only genuinely scattered answers: below a quarter, the
    /// probability is spread across three or more levels of five, and that is
    /// a shrug worth storing and not worth drawing.
    pub fn firm(&self) -> bool {
        self.sc >= 0.25
    }

    /// Whether the model knows what this message *is*, whatever it thinks of
    /// how unwanted it is.
    ///
    /// The two questions are answered separately and come back with separate
    /// confidences, and on real mail they diverge hard: a sale email is
    /// `category promotional, confidence 0.99` and `score 2.06, confidence
    /// 0.21` at the same time. The model is certain what it is and genuinely
    /// split on whether it is a level 2 or a level 3, which is a judgement
    /// rather than a fact.
    ///
    /// So this is the confidence that gates a label. A message the model
    /// cannot classify *and* cannot place on the scale is the only real shrug.
    pub fn classified(&self) -> bool {
        self.cc >= 0.5
    }
}

/// Where one message's verdict lives.
///
/// Keyed by the message's own `Message-ID` rather than by the row it happens
/// to occupy. The same email arrives in an inbox and is copied into an archive
/// or an "All Mail" folder, and those are two rows with two ids and one
/// message — so a local key pays twice to learn the same thing, and then draws
/// the two copies differently when the second reading lands somewhere else on
/// the scale.
///
/// The header costs a call that a purely local key would not, which is the
/// trade: one indexed lookup per decorated row against one API request per
/// duplicate. On a mailbox where nearly everything exists twice that is not
/// close.
///
/// Mail without a `Message-ID` falls back to the row. It is rare, it is
/// usually something locally generated, and two copies of it are two
/// separate problems anyway.
fn key(message: u64) -> String {
    match host::header(message, "message-id") {
        Some(rfc) if !rfc.trim().is_empty() => format!("{VERSION}.m.{}", rfc.trim()),
        _ => format!("{VERSION}.local.{message}"),
    }
}

pub fn load(message: u64) -> Option<Verdict> {
    let stored = host::store_get(&key(message))?;
    match serde_json::from_str(&stored) {
        Ok(verdict) => Some(verdict),
        // A value that will not parse is one this version cannot use. Dropping
        // it rather than returning nothing forever means the message is scored
        // again and the bad row is replaced.
        Err(_) => {
            host::store_delete(&key(message));
            None
        }
    }
}

/// A message's verdict, if it was reached under the labels as they are now —
/// the test for whether a message still needs scoring.
pub fn current(message: u64, fingerprint: &str) -> Option<Verdict> {
    load(message).filter(|v| v.is_current(fingerprint))
}

pub fn save(message: u64, verdict: &Verdict) {
    match serde_json::to_string(verdict) {
        Ok(json) => host::store_set(&key(message), &json),
        Err(e) => host::log(host::Level::Warn, &format!("could not store a verdict: {e}")),
    }
}

/// The running totals the panel draws.
///
/// Kept as a tally rather than counted on demand because there is no way to
/// count: the key/value store answers for a key you name and has no way to be
/// asked what it holds. A number that is maintained is the only number
/// available, so it is maintained.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tally {
    pub scored: u64,
    /// Readings at or above the threshold.
    pub spam: u64,
    pub impersonations: u64,
    /// Input tokens. Output is not billed; Jev returns decisions, not text.
    pub tokens: u64,
    pub errors: u64,
    /// The last thing that went wrong, for the panel to show. One line, not a
    /// log: an extension that kept a history here would be keeping it forever.
    pub last_error: String,
    pub last_run: i64,
}

const TALLY: &str = "tally";

impl Tally {
    pub fn load() -> Self {
        host::store_get(TALLY)
            .and_then(|stored| serde_json::from_str(&stored).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            host::store_set(TALLY, &json);
        }
    }

    pub fn record(&mut self, verdict: &Verdict, threshold: f64) {
        self.scored += 1;
        if verdict.s >= threshold {
            self.spam += 1;
        }
        if verdict.impersonates() {
            self.impersonations += 1;
        }
    }

    pub fn failed(&mut self, why: &str) {
        self.errors += 1;
        self.last_error = why.to_string();
    }
}
