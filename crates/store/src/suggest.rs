//! Who you might be writing to.
//!
//! The mail says who you actually correspond with, and that is the whole of
//! it: every address that has been in a header of yours, ranked.
//!
//! Ranking is frequency and recency, in that order of influence. That is a
//! deliberately blunt rule: the right answer for a two-letter prefix is nearly
//! always somebody you wrote to last week, and anything cleverer is a model
//! that will be wrong in ways that are hard to explain.

use anyhow::Result;
use spool_core::{Address, address};

use crate::Store;

/// One address the composer can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub address: Address,
    /// What put it at this position. Ranking only; never shown.
    pub score: i64,
}

/// How many messages are read to build the history half.
///
/// A bound rather than the whole mailbox: the addresses worth suggesting are
/// in the recent end of it, and this runs on every few keystrokes. Someone you
/// have not written to in ten thousand messages is someone you will type out.
const HISTORY_DEPTH: usize = 4_000;

impl Store {
    /// Addresses matching `prefix`, best first.
    ///
    /// The prefix is matched against both the display name and the address, so
    /// `ana` finds `Ana Ruiz <a.ruiz@example.com>` and `ruiz` finds her too.
    pub fn address_suggestions(&self, prefix: &str, limit: usize) -> Result<Vec<Suggestion>> {
        let needle = prefix.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut found: Vec<Candidate> = Vec::new();
        self.correspondents(&needle, &mut found)?;

        Ok(rank(found, limit))
    }

    /// Everyone in the recent mail whose name or address contains the needle.
    ///
    /// One scan with a `LIKE` prefilter rather than a maintained index of
    /// addresses. The columns hold encoded address *lists*, so the database
    /// cannot answer this on its own — it can only say which rows are worth
    /// decoding, which is what this asks it.
    fn correspondents(&self, needle: &str, into: &mut Vec<Candidate>) -> Result<()> {
        let this = self.lock();
        let like = format!("%{}%", escape_like(needle));

        let mut stmt = this.conn.prepare(
            "SELECT from_name, from_addr, to_addrs, cc_addrs, date_ts
             FROM messages
             WHERE from_name  LIKE ?1 ESCAPE '\\'
                OR from_addr  LIKE ?1 ESCAPE '\\'
                OR to_addrs   LIKE ?1 ESCAPE '\\'
                OR cc_addrs   LIKE ?1 ESCAPE '\\'
             ORDER BY date_ts DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![like, HISTORY_DEPTH as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        for row in rows {
            let (from_name, from_addr, to_addrs, cc_addrs, date) = row?;

            let sender = Address::new(from_name, from_addr);
            for address in std::iter::once(sender)
                .chain(address::decode_list(&to_addrs))
                .chain(address::decode_list(&cc_addrs))
            {
                if matches(&address, needle) {
                    into.push(Candidate { address, last_seen: date });
                }
            }
        }
        Ok(())
    }
}

/// One sighting of an address, before they are counted up.
struct Candidate {
    address: Address,
    last_seen: i64,
}

/// Whether the needle appears in either half of an address.
///
/// Both halves, because people search for whichever they remember: the name of
/// the person or the string they have seen in a header.
fn matches(address: &Address, needle: &str) -> bool {
    !address.addr.trim().is_empty()
        && (address.addr.to_lowercase().contains(needle)
            || address.name.to_lowercase().contains(needle))
}

/// Folds sightings into one entry per address and orders them.
///
/// Separated from the queries so the ordering can be tested without a
/// database, which is the part with a judgement in it.
fn rank(candidates: Vec<Candidate>, limit: usize) -> Vec<Suggestion> {
    /// One address as it is being counted up.
    struct Tally {
        suggestion: Suggestion,
        last_seen: i64,
    }

    let mut merged: Vec<(String, Tally)> = Vec::new();

    for candidate in candidates {
        let key = address::normalize(&candidate.address.addr);
        if key.is_empty() {
            continue;
        }

        match merged.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, tally)) => {
                // Every sighting is worth one.
                tally.suggestion.score += 1;
                tally.last_seen = tally.last_seen.max(candidate.last_seen);

                // A named sighting beats a bare address: `Ana Ruiz
                // <ana@example.com>` is what to offer, even if the first time
                // this address was seen it arrived without a name.
                if tally.suggestion.address.name.trim().is_empty()
                    && !candidate.address.name.trim().is_empty()
                {
                    tally.suggestion.address.name = candidate.address.name;
                }
            }
            None => merged.push((
                key,
                Tally {
                    suggestion: Suggestion { address: candidate.address, score: 1 },
                    last_seen: candidate.last_seen,
                },
            )),
        }
    }

    // Frequency first, then recency, then the address itself so the order is
    // stable rather than dependent on how the rows came back.
    merged.sort_by(|a, b| {
        b.1.suggestion
            .score
            .cmp(&a.1.suggestion.score)
            .then(b.1.last_seen.cmp(&a.1.last_seen))
            .then(a.1.suggestion.address.addr.cmp(&b.1.suggestion.address.addr))
    });

    merged.into_iter().take(limit).map(|(_, tally)| tally.suggestion).collect()
}

/// Escapes the wildcards so a needle containing `%` or `_` matches itself.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(name: &str, addr: &str, at: i64) -> Candidate {
        Candidate { address: Address::new(name, addr), last_seen: at }
    }

    fn addrs(suggestions: &[Suggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.address.addr.as_str()).collect()
    }

    #[test]
    fn who_you_write_to_most_comes_first() {
        let ranked = rank(
            vec![
                seen("Bo", "bo@example.com", 10),
                seen("Ana", "ana@example.com", 5),
                seen("Ana", "ana@example.com", 6),
                seen("Ana", "ana@example.com", 7),
            ],
            10,
        );
        assert_eq!(addrs(&ranked), ["ana@example.com", "bo@example.com"]);
    }

    #[test]
    fn recency_breaks_a_tie() {
        let ranked =
            rank(vec![seen("Bo", "bo@example.com", 900), seen("Ana", "ana@example.com", 100)], 10);
        assert_eq!(addrs(&ranked), ["bo@example.com", "ana@example.com"]);
    }

    #[test]
    fn one_person_is_one_row_however_they_spelled_their_address() {
        let ranked = rank(
            vec![
                seen("Ana", "Ana@Example.COM", 1),
                seen("Ana", "ana@example.com", 2),
                seen("", "ANA@EXAMPLE.COM", 3),
            ],
            10,
        );
        assert_eq!(ranked.len(), 1, "{ranked:#?}");
        assert_eq!(ranked[0].score, 3);
    }

    #[test]
    fn a_name_is_kept_even_if_the_address_was_first_seen_without_one() {
        // Offering a bare address when the name is known somewhere in the
        // mailbox is the kind of small wrongness that makes a feature feel
        // unfinished.
        let ranked = rank(
            vec![seen("", "ana@example.com", 5), seen("Ana Ruiz", "ana@example.com", 1)],
            10,
        );
        assert_eq!(ranked[0].address.name, "Ana Ruiz");
    }

    #[test]
    fn an_entry_with_no_address_is_not_offered() {
        // `Undisclosed recipients:;` and other header debris decode to a name
        // with nothing to send to.
        let ranked = rank(vec![seen("Undisclosed recipients", "", 1)], 10);
        assert!(ranked.is_empty());
    }

    #[test]
    fn the_limit_is_respected() {
        let candidates: Vec<Candidate> =
            (0..30).map(|i| seen("X", &format!("x{i}@example.com"), i)).collect();
        assert_eq!(rank(candidates, 5).len(), 5);
    }

    #[test]
    fn a_needle_with_a_wildcard_in_it_matches_itself() {
        // `%` is legal in the local part of an address, and unescaped it would
        // turn one typo into a query that matches the entire mailbox.
        assert_eq!(escape_like("a%b"), "a\\%b");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn suggestions_come_back_from_a_real_mailbox() {
        use crate::IncomingMessage;
        use spool_core::MailboxRole;

        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();

        let message = |uid: u32, from: Address, to: Vec<Address>| IncomingMessage {
            account_id: account,
            mailbox_id: inbox,
            uid: Some(uid),
            rfc_message_id: format!("<{uid}@example.com>"),
            from,
            to,
            subject: "Hello".into(),
            date: 1_700_000_000 + uid as i64,
            ..Default::default()
        };

        store
            .ingest(&[
                message(1, Address::new("Ana Ruiz", "ana@example.com"), vec![]),
                message(2, Address::new("Ana Ruiz", "ana@example.com"), vec![]),
                message(
                    3,
                    Address::new("Bo Chen", "bo@example.com"),
                    // A recipient, not a sender: the encoded list has to be
                    // decoded for this to be found at all.
                    vec![Address::new("Cass Ando", "cass@example.com")],
                ),
            ])
            .unwrap();

        let ana = store.address_suggestions("ana", 10).unwrap();
        assert_eq!(ana.len(), 1);
        assert_eq!(ana[0].address.addr, "ana@example.com");
        assert_eq!(ana[0].address.name, "Ana Ruiz");
        assert_eq!(ana[0].score, 2, "seen twice");

        let cass = store.address_suggestions("cass", 10).unwrap();
        assert_eq!(cass.len(), 1, "someone written *to* is a correspondent too");
        assert_eq!(cass[0].address.addr, "cass@example.com");

        // Matched on the address as well as the name.
        assert_eq!(store.address_suggestions("bo@exa", 10).unwrap().len(), 1);

        assert!(store.address_suggestions("nobody", 10).unwrap().is_empty());
        assert!(store.address_suggestions("", 10).unwrap().is_empty(), "an empty prefix asks for everyone");
    }

    #[test]
    fn a_needle_matches_either_half_of_an_address() {
        let ana = Address::new("Ana Ruiz", "a.ruiz@example.com");
        assert!(matches(&ana, "ana"), "the name people remember");
        assert!(matches(&ana, "ruiz"), "either part of it");
        assert!(matches(&ana, "a.ruiz"), "or the address they have seen");
        assert!(!matches(&ana, "bo"));
    }
}
