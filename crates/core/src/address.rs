//! Email addresses and the small amount of formatting the UI needs from them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub addr: String,
}

impl Address {
    pub fn new(name: impl Into<String>, addr: impl Into<String>) -> Self {
        Self { name: name.into(), addr: addr.into() }
    }

    pub fn bare(addr: impl Into<String>) -> Self {
        Self { name: String::new(), addr: addr.into() }
    }

    /// What the message list shows: the display name if there is one, else the
    /// local part of the address (`ana@example.com` reads better as `ana` than
    /// as the full address in a narrow column).
    pub fn short(&self) -> &str {
        if !self.name.is_empty() {
            &self.name
        } else if let Some((local, _)) = self.addr.split_once('@') {
            local
        } else {
            &self.addr
        }
    }

    /// The part after the `@`, lowercased, or `None` when the address has no
    /// domain to speak of.
    ///
    /// Owned and lowercased rather than borrowed: domains are
    /// case-insensitive, and a rule stored as `News.Temu.com` that fails to
    /// match `news.temu.com` the next time the same sender writes is worse
    /// than useless. The exact host, not a guess at the organisation behind
    /// it: `news.temu.com` and `temu.com` are different senders, and deciding
    /// they are the same means trusting a subdomain nobody agreed to.
    pub fn domain(&self) -> Option<String> {
        let (_, domain) = self.addr.rsplit_once('@')?;
        let domain = domain.trim().trim_end_matches('>').to_ascii_lowercase();
        (!domain.is_empty()).then_some(domain)
    }

    /// `Ana Ruiz <ana@example.com>`, or just the address when unnamed.
    pub fn full(&self) -> String {
        if self.name.is_empty() {
            self.addr.clone()
        } else {
            format!("{} <{}>", self.name, self.addr)
        }
    }

    /// The address as it goes into a header or a stored list.
    ///
    /// Unlike [`Self::full`], this quotes a display name containing anything
    /// that address syntax would otherwise read as structure. Without it a
    /// name like `Ruiz, Ana` is split at the comma the next time the list is
    /// parsed, and the person silently becomes `Ana`.
    pub fn encoded(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            return self.addr.trim().to_string();
        }

        if name.contains(['"', ',', '<', '>', ':', ';', '@', '\\', '(', ')', '[', ']']) {
            let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\" <{}>", self.addr.trim())
        } else {
            format!("{name} <{}>", self.addr.trim())
        }
    }

    /// One or two letters for the avatar chip in the list.
    pub fn initials(&self) -> String {
        let source = if self.name.is_empty() { self.short() } else { &self.name };
        let mut words = source
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty());

        let first = words.next().and_then(|w| w.chars().next());
        let second = words.next().and_then(|w| w.chars().next());

        match (first, second) {
            (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
            (Some(a), None) => a.to_uppercase().to_string(),
            _ => "?".into(),
        }
    }

    /// Stable hash of the address, used to pick an avatar color. Deliberately
    /// keyed on the address rather than the name so the same person keeps the
    /// same color even when they change how they sign their mail.
    pub fn color_seed(&self) -> u32 {
        let mut hash: u32 = 2_166_136_261;
        for byte in self.addr.bytes() {
            hash ^= byte.to_ascii_lowercase() as u32;
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full())
    }
}

/// The form an address is compared in: trimmed and lowercased.
///
/// Mail addresses are case-insensitive in the domain and, in every practical
/// sense, in the local part too — nobody has two correspondents who differ
/// only in the capitals, and treating them as two people is how a list of
/// them ends up with duplicates.
pub fn normalize(addr: &str) -> String {
    addr.trim().to_lowercase()
}

/// Serializes a list of addresses for storage in a single text column.
///
/// Uses [`Address::encoded`] rather than [`Address::full`] so the result can be
/// parsed back: `full` is for showing a person, this is for storing them.
pub fn encode_list(addrs: &[Address]) -> String {
    addrs.iter().map(Address::encoded).collect::<Vec<_>>().join(", ")
}

/// Parses the format written by [`encode_list`], and by extension the common
/// case of a raw `To:` header.
pub fn decode_list(s: &str) -> Vec<Address> {
    split_addresses(s).filter_map(parse_one).collect()
}

/// Splits a list on the commas that separate addresses, keeping each part as
/// it was written.
///
/// Public because a recipient field needs the same splitting as [`decode_list`]
/// but has to keep the parts that do *not* parse: an address half typed is
/// still something the person put there, and dropping it because it has no `@`
/// yet would delete what they were in the middle of writing.
pub fn split_list(s: &str) -> impl Iterator<Item = &str> {
    split_addresses(s)
}

/// Splits on commas that are not inside quotes or angle brackets.
fn split_addresses(s: &str) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let (mut start, mut in_quotes, mut in_angles) = (0usize, false, false);

    for (ix, ch) in s.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => in_angles = true,
            '>' if !in_quotes => in_angles = false,
            ',' if !in_quotes && !in_angles => {
                parts.push(&s[start..ix]);
                start = ix + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter().map(str::trim).filter(|p| !p.is_empty())
}

fn parse_one(part: &str) -> Option<Address> {
    if let Some(open) = part.rfind('<') {
        let close = part[open..].find('>')? + open;
        let addr = part[open + 1..close].trim();
        let name = part[..open].trim().trim_matches('"').trim();
        (!addr.is_empty()).then(|| Address::new(name, addr))
    } else if part.contains('@') {
        Some(Address::bare(part))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_senders_domain_is_the_host_they_wrote_from() {
        assert_eq!(Address::bare("ana@Example.COM").domain().as_deref(), Some("example.com"));
        assert_eq!(
            Address::new("Temu", "email@news.temu.com").domain().as_deref(),
            Some("news.temu.com"),
            "the exact host, not the registered domain behind it",
        );
    }

    #[test]
    fn an_address_with_no_domain_has_none() {
        assert_eq!(Address::bare("postmaster").domain(), None);
        assert_eq!(Address::bare("broken@").domain(), None);
        assert_eq!(Address::bare("").domain(), None);
    }

    #[test]
    fn an_address_with_two_at_signs_takes_the_last_one() {
        // Quoted local parts can contain an `@`, and the domain is what
        // follows the final one.
        assert_eq!(
            Address::bare(r#""odd@name"@example.com"#).domain().as_deref(),
            Some("example.com"),
        );
    }

    #[test]
    fn short_prefers_name_then_local_part() {
        assert_eq!(Address::new("Ana Ruiz", "ana@example.com").short(), "Ana Ruiz");
        assert_eq!(Address::bare("ana@example.com").short(), "ana");
    }

    #[test]
    fn initials_use_up_to_two_words() {
        assert_eq!(Address::new("Ana Ruiz", "a@b.com").initials(), "AR");
        assert_eq!(Address::new("Ana", "a@b.com").initials(), "A");
        assert_eq!(Address::bare("ana@b.com").initials(), "A");
    }

    #[test]
    fn a_name_containing_a_comma_survives_a_round_trip() {
        // "Last, First" display names are common, and splitting on the comma
        // silently renames the person.
        let addrs = vec![
            Address::new("Ruiz, Ana", "ana@example.com"),
            Address::bare("bo@example.com"),
        ];
        assert_eq!(decode_list(&encode_list(&addrs)), addrs);
    }

    #[test]
    fn names_needing_quotes_get_them_and_others_do_not() {
        assert_eq!(
            Address::new("Ruiz, Ana", "a@b.com").encoded(),
            r#""Ruiz, Ana" <a@b.com>"#,
        );
        assert_eq!(Address::new("Ana Ruiz", "a@b.com").encoded(), "Ana Ruiz <a@b.com>");
        assert_eq!(Address::bare("a@b.com").encoded(), "a@b.com");
    }

    #[test]
    fn a_quote_inside_a_name_is_escaped() {
        let addr = Address::new(r#"Ana "The Closer" Ruiz"#, "a@b.com");
        let decoded = decode_list(&encode_list(std::slice::from_ref(&addr)));

        assert_eq!(decoded.len(), 1, "escaping should not split the address");
        assert_eq!(decoded[0].addr, "a@b.com");
    }

    #[test]
    fn display_and_encoded_forms_differ_where_they_should() {
        let addr = Address::new("Ruiz, Ana", "a@b.com");
        // `full` is what a person reads; it stays unquoted.
        assert_eq!(addr.full(), "Ruiz, Ana <a@b.com>");
        assert!(addr.encoded().starts_with('"'));
    }

    #[test]
    fn list_round_trips_through_encoding() {
        let addrs = vec![
            Address::new("Ana Ruiz", "ana@example.com"),
            Address::bare("bo@example.com"),
        ];
        assert_eq!(decode_list(&encode_list(&addrs)), addrs);
    }

    #[test]
    fn commas_inside_quoted_names_do_not_split() {
        let parsed = decode_list("\"Ruiz, Ana\" <ana@example.com>, bo@example.com");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Ruiz, Ana");
        assert_eq!(parsed[1].addr, "bo@example.com");
    }

    #[test]
    fn same_address_always_gets_the_same_color() {
        let a = Address::new("Ana", "ana@example.com");
        let b = Address::new("Ana Ruiz", "ANA@example.com");
        assert_eq!(a.color_seed(), b.color_seed());
    }
}
