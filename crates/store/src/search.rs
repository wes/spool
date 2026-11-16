//! The search box, parsed.
//!
//! People type prose, and occasionally they type an operator. The rule that
//! governs everything here is that **nothing typed into the box is an error**:
//! an unbalanced quote, a stray colon, a date that is not a date — all of it
//! has to come back as a search that runs, because a mail client that refuses
//! to search while you are still typing is a mail client you stop typing into.
//!
//! So an operator this file does not recognise is not rejected; it falls
//! through and becomes ordinary search text. `chore:` searches for "chore".

use spool_core::{Flags, unix_from_civil};
use rusqlite::types::Value;

/// A parsed search box: the free text, and everything used to narrow it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Search {
    /// Everything that was not an operator, for the full-text index.
    pub text: String,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: Vec<String>,
    /// `in:` — matched against a mailbox's name and its server path.
    pub folder: Vec<String>,
    pub has_attachment: bool,
    /// `is:read` / `is:unread`.
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    /// Unix seconds. `after:` is inclusive of the day named, `before:` is not.
    pub after: Option<i64>,
    pub before: Option<i64>,
}

/// The operators, and what they are called in the search box.
///
/// Listed here rather than only in the parser so the UI can show them without
/// a second copy of the truth.
pub const OPERATORS: &[(&str, &str)] = &[
    ("from:", "sender name or address"),
    ("to:", "recipient, including Cc"),
    ("subject:", "words in the subject"),
    ("in:", "folder name"),
    ("has:attachment", "carries a file"),
    ("is:unread", "also is:read"),
    ("is:flagged", "also is:unflagged"),
    ("after:", "2026-01-31, or 7d"),
    ("before:", "2026-01-31, or 7d"),
];

impl Search {
    /// Parses a search box. `now` is unix seconds, for the relative dates.
    pub fn parse(input: &str, now: i64) -> Self {
        let mut search = Search::default();
        let mut text: Vec<String> = Vec::new();

        for token in tokens(input) {
            match split_operator(&token) {
                Some((key, value)) => {
                    if !search.apply(key, value, now) {
                        // Not an operator after all. The whole token goes back
                        // into the text so that typing a URL, or a time like
                        // `10:30`, searches for what was typed.
                        text.push(token.clone());
                    }
                }
                None => text.push(token.clone()),
            }
        }

        search.text = text.join(" ");
        search
    }

    /// Applies one operator. Returns whether it was one.
    fn apply(&mut self, key: &str, value: &str, now: i64) -> bool {
        if value.is_empty() {
            return false;
        }

        match key {
            "from" => self.from.push(value.to_string()),
            "to" => self.to.push(value.to_string()),
            "subject" => self.subject.push(value.to_string()),
            "in" | "folder" => self.folder.push(value.to_string()),
            "has" => match value.to_ascii_lowercase().as_str() {
                "attachment" | "attachments" | "file" | "files" => self.has_attachment = true,
                _ => return false,
            },
            "is" => match value.to_ascii_lowercase().as_str() {
                "unread" | "new" => self.seen = Some(false),
                "read" => self.seen = Some(true),
                "flagged" | "starred" => self.flagged = Some(true),
                "unflagged" | "unstarred" => self.flagged = Some(false),
                _ => return false,
            },
            "after" | "since" => match parse_date(value, now) {
                Some(ts) => self.after = Some(ts),
                None => return false,
            },
            "before" | "until" => match parse_date(value, now) {
                Some(ts) => self.before = Some(ts),
                None => return false,
            },
            _ => return false,
        }
        true
    }

    /// Whether this narrows anything at all.
    pub fn is_empty(&self) -> bool {
        *self == Search::default()
    }

    /// The FTS5 query for the free text, if there is any.
    pub fn fts(&self) -> Option<String> {
        let query = to_fts_query(&self.text);
        (!query.is_empty()).then_some(query)
    }

    /// The `AND …` clauses these operators add, pushing their values onto
    /// `params` in the order the placeholders appear.
    pub fn clauses(&self, params: &mut Vec<Value>) -> String {
        let mut sql = String::new();

        // Sender and recipient are matched against name and address together:
        // `from:ana` should find Ana Ruiz whether you remember her name or her
        // address, and nobody types the one they are less sure of.
        for name in &self.from {
            sql.push_str(" AND (m.from_addr LIKE ? ESCAPE '\\' OR m.from_name LIKE ? ESCAPE '\\')");
            params.push(like(name).into());
            params.push(like(name).into());
        }
        for name in &self.to {
            sql.push_str(" AND (m.to_addrs LIKE ? ESCAPE '\\' OR m.cc_addrs LIKE ? ESCAPE '\\')");
            params.push(like(name).into());
            params.push(like(name).into());
        }
        for words in &self.subject {
            sql.push_str(" AND m.subject LIKE ? ESCAPE '\\'");
            params.push(like(words).into());
        }
        for folder in &self.folder {
            sql.push_str(
                " AND m.mailbox_id IN (SELECT id FROM mailboxes
                     WHERE name LIKE ? ESCAPE '\\' OR path LIKE ? ESCAPE '\\')",
            );
            params.push(like(folder).into());
            params.push(like(folder).into());
        }

        if self.has_attachment {
            sql.push_str(" AND m.has_attachments != 0");
        }
        match self.seen {
            Some(true) => sql.push_str(&format!(" AND m.flags & {} != 0", Flags::SEEN)),
            Some(false) => sql.push_str(&format!(" AND m.flags & {} = 0", Flags::SEEN)),
            None => {}
        }
        match self.flagged {
            Some(true) => sql.push_str(&format!(" AND m.flags & {} != 0", Flags::FLAGGED)),
            Some(false) => sql.push_str(&format!(" AND m.flags & {} = 0", Flags::FLAGGED)),
            None => {}
        }

        if let Some(after) = self.after {
            sql.push_str(" AND m.date_ts >= ?");
            params.push(after.into());
        }
        if let Some(before) = self.before {
            sql.push_str(" AND m.date_ts < ?");
            params.push(before.into());
        }

        sql
    }
}

/// Splits the box into tokens, keeping a quoted phrase together.
///
/// Quotes are consumed rather than required to balance: `subject:"the roof`
/// is a token, not a syntax error, because the closing quote is simply one the
/// user has not typed yet.
fn tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Splits `key:value`, lowercasing the key. `None` when there is no colon, or
/// nothing before it.
fn split_operator(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    // Alphabetic only, so `10:30` and `https://…` are never mistaken for one.
    let is_key = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic());
    is_key.then(|| (key, value.trim()))
}

/// A date operator's value, as unix seconds at midnight UTC.
///
/// Absolute (`2026-01-31`, `2026-01`, `2026`) or relative (`7d`, `2w`, `today`,
/// `yesterday`). Anything else is `None`, which sends the whole token back to
/// the search text — searching for what was typed beats silently ignoring it.
fn parse_date(value: &str, now: i64) -> Option<i64> {
    let value = value.trim().to_ascii_lowercase();
    let midnight = now.div_euclid(86_400) * 86_400;

    match value.as_str() {
        "today" => return Some(midnight),
        "yesterday" => return Some(midnight - 86_400),
        _ => {}
    }

    // Relative: a count and a unit, counted back from today.
    if let Some(unit) = value.chars().last().filter(|c| "dwmy".contains(*c))
        && let Ok(count) = value[..value.len() - 1].parse::<i64>()
        && count >= 0
    {
        let days = match unit {
            'd' => count,
            'w' => count * 7,
            'm' => count * 30,
            _ => count * 365,
        };
        return Some(midnight - days * 86_400);
    }

    let parts: Vec<&str> = value.split(['-', '/', '.']).collect();
    let year: i64 = parts.first()?.parse().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let month: i64 = match parts.get(1) {
        Some(part) => part.parse().ok().filter(|m| (1..=12).contains(m))?,
        None => 1,
    };
    let day: i64 = match parts.get(2) {
        Some(part) => part.parse().ok().filter(|d| (1..=31).contains(d))?,
        None => 1,
    };

    Some(unix_from_civil(year, month, day))
}

/// Wraps a value for a `LIKE`, escaping the wildcards it may contain.
pub(crate) fn like(value: &str) -> String {
    let escaped = value.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_");
    format!("%{escaped}%")
}

/// Rewrites free text into an FTS5 query.
///
/// Each word is quoted and given a prefix wildcard, so results narrow as you
/// type and an unbalanced quote or a bare `-` cannot become a syntax error.
pub fn to_fts_query(input: &str) -> String {
    let terms: Vec<String> = input
        .split_whitespace()
        .filter_map(|word| {
            let cleaned: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '-' | '_'))
                .collect();
            (!cleaned.is_empty()).then(|| format!("\"{cleaned}\"*"))
        })
        .collect();

    terms.join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-05, the day these were written.
    const NOW: i64 = 1_757_030_400;

    fn parse(input: &str) -> Search {
        Search::parse(input, NOW)
    }

    #[test]
    fn plain_words_are_still_just_words() {
        let search = parse("quarterly report");
        assert_eq!(search.text, "quarterly report");
        assert!(search.from.is_empty());
        assert_eq!(search.fts().unwrap(), "\"quarterly\"* AND \"report\"*");
    }

    #[test]
    fn operators_are_pulled_out_and_the_rest_is_text() {
        let search = parse("from:ana subject:roof leak");
        assert_eq!(search.from, ["ana"]);
        assert_eq!(search.subject, ["roof"]);
        assert_eq!(search.text, "leak");
    }

    #[test]
    fn a_quoted_phrase_stays_one_value() {
        let search = parse("subject:\"quarterly report\" from:\"Ruiz, Ana\"");
        assert_eq!(search.subject, ["quarterly report"]);
        assert_eq!(search.from, ["Ruiz, Ana"]);
        assert_eq!(search.text, "");
    }

    #[test]
    fn an_unbalanced_quote_is_a_search_not_an_error() {
        // The closing quote is one the user has not typed yet.
        let search = parse("subject:\"the roof");
        assert_eq!(search.subject, ["the roof"]);
    }

    #[test]
    fn state_operators_read_the_flags() {
        assert_eq!(parse("is:unread").seen, Some(false));
        assert_eq!(parse("is:read").seen, Some(true));
        assert_eq!(parse("is:flagged").flagged, Some(true));
        assert_eq!(parse("is:unflagged").flagged, Some(false));
        assert!(parse("has:attachment").has_attachment);
        assert!(parse("has:files").has_attachment);
    }

    #[test]
    fn an_unknown_operator_becomes_ordinary_text() {
        // Colons turn up in prose, in URLs and in times. None of them should
        // narrow a search to nothing, and none of them should be an error.
        for input in ["chore:", "https://example.com/x", "10:30", "is:sideways", "has:legs"] {
            let search = parse(input);
            assert_eq!(search.text, input, "{input:?} should have stayed text");
            assert!(search.is_empty() || !search.text.is_empty());
        }
    }

    #[test]
    fn a_key_with_no_value_is_text() {
        assert_eq!(parse("from:").text, "from:");
    }

    #[test]
    fn absolute_dates_land_on_midnight_utc() {
        assert_eq!(parse("after:2026-01-31").after, Some(unix_from_civil(2026, 1, 31)));
        assert_eq!(parse("before:2026/01/31").before, Some(unix_from_civil(2026, 1, 31)));
        // A month or a year alone means its first day.
        assert_eq!(parse("after:2026-02").after, Some(unix_from_civil(2026, 2, 1)));
        assert_eq!(parse("after:2026").after, Some(unix_from_civil(2026, 1, 1)));
    }

    #[test]
    fn relative_dates_count_back_from_today() {
        assert_eq!(parse("after:today").after, Some(NOW));
        assert_eq!(parse("after:yesterday").after, Some(NOW - 86_400));
        assert_eq!(parse("after:7d").after, Some(NOW - 7 * 86_400));
        assert_eq!(parse("after:2w").after, Some(NOW - 14 * 86_400));
    }

    #[test]
    fn a_date_that_is_not_a_date_searches_for_what_was_typed() {
        // Silently dropping the operator would show results that ignore half
        // of what was asked for, with nothing on screen saying so.
        let search = parse("after:soon");
        assert_eq!(search.after, None);
        assert_eq!(search.text, "after:soon");

        assert_eq!(parse("after:2026-13-01").text, "after:2026-13-01");
        assert_eq!(parse("before:1066").text, "before:1066");
    }

    #[test]
    fn the_same_operator_twice_narrows_twice() {
        let search = parse("subject:roof subject:leak");
        assert_eq!(search.subject, ["roof", "leak"]);
    }

    #[test]
    fn wildcards_in_a_value_are_escaped_rather_than_matching_everything() {
        // Without the escape, `from:%` matches every message ever received.
        let mut params = Vec::new();
        parse("from:100%").clauses(&mut params);
        assert_eq!(params[0], Value::Text(r"%100\%%".into()));
    }

    #[test]
    fn clauses_bind_one_parameter_per_placeholder() {
        // A mismatch here is the bug that makes a search return the wrong mail
        // rather than an error, so it is worth counting.
        let mut params = Vec::new();
        let sql = parse("from:ana to:bo subject:roof in:work after:2026-01-01 before:2026-02-01")
            .clauses(&mut params);
        assert_eq!(sql.matches('?').count(), params.len());
    }

    #[test]
    fn an_empty_box_narrows_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
        assert!(!parse("is:unread").is_empty());
    }
}
