//! Contacts arriving from outside: a CSV somebody exported, a vCard file, a
//! JSON feed of signups.
//!
//! Three formats, one shape. Each is read into a [`Table`] of named columns,
//! the columns are given meanings — this one is the address, that one the name
//! — and the rows become [`Contact`]s. The middle step is where the judgement
//! is, so it is the step a person can see and correct: [`guess`] proposes, the
//! import sheet shows its proposal, and whatever they change it to is what
//! [`contacts`] reads.
//!
//! Nothing here fails on a bad row. A file exported from a spreadsheet is
//! a file with a blank line in it, a stray quote, a column nobody filled in;
//! the row that cannot be read becomes a contact with no address, and the book
//! counts it as skipped, which is what the person needs to hear.

use serde_json::Value;

/// One person, as an import brings them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contact {
    pub email: String,
    /// More addresses that are the same person, after the first.
    pub other_emails: Vec<String>,
    pub name: String,
    pub company: String,
    /// What the source calls this person, so a second pull finds them again
    /// even where the address has changed.
    pub external_id: String,
    pub tags: Vec<String>,
    /// Everything else worth keeping, by column name.
    pub fields: Vec<(String, String)>,
}

/// Rows of text under named columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// The first few non-empty values in a column, for showing beside it.
    pub fn samples(&self, column: usize, count: usize) -> Vec<&str> {
        self.rows
            .iter()
            .filter_map(|row| row.get(column).map(|v| v.trim()))
            .filter(|v| !v.is_empty())
            .take(count)
            .collect()
    }
}

/// What a column means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    Email,
    Name,
    FirstName,
    LastName,
    Company,
    /// The source's own id for the person.
    Id,
    Tags,
    /// Kept as a field under the column's own name.
    Field,
    Skip,
}

impl Column {
    /// Every meaning, in the order a picker offers them.
    pub const ALL: [Column; 9] = [
        Column::Email,
        Column::Name,
        Column::FirstName,
        Column::LastName,
        Column::Company,
        Column::Id,
        Column::Tags,
        Column::Field,
        Column::Skip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Column::Email => "Email",
            Column::Name => "Name",
            Column::FirstName => "First name",
            Column::LastName => "Last name",
            Column::Company => "Company",
            Column::Id => "Their id",
            Column::Tags => "Tags",
            Column::Field => "Keep as a field",
            Column::Skip => "Ignore",
        }
    }
}

/// Reads a CSV, whichever of the usual separators it uses.
///
/// Quoted fields may hold the separator, newlines and doubled quotes, which
/// is the whole of what RFC 4180 says and most of what exporters do. The
/// separator is whichever of comma, semicolon or tab the header line has most
/// of outside quotes: Excel in a European locale writes semicolons, and a
/// file read with the wrong one is a file with one column.
pub fn parse_csv(text: &str) -> Table {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let separator = sniff_separator(text);

    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') if field.trim().is_empty() => {
                field.clear();
                quoted = true;
            }
            (false, c) if c == separator => record.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }

    // Blank lines are not rows.
    records.retain(|r| r.iter().any(|f| !f.trim().is_empty()));
    let mut records = records.into_iter();
    let headers: Vec<String> = records.next().unwrap_or_default().into_iter().map(|h| h.trim().to_string()).collect();
    Table { headers, rows: records.collect() }
}

fn sniff_separator(text: &str) -> char {
    let mut quoted = false;
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => break,
            c if !quoted => {
                if let Some(entry) = counts.iter_mut().find(|(s, _)| *s == c) {
                    entry.1 += 1;
                }
            }
            _ => {}
        }
    }
    counts.iter().max_by_key(|(_, n)| *n).filter(|(_, n)| *n > 0).map(|(s, _)| *s).unwrap_or(',')
}

/// Reads a JSON feed into a table: one row per object.
///
/// The objects are found wherever a feed usually keeps them — the top level,
/// or under `data`, `users`, `results` or the like — and nested objects are
/// flattened one level into dotted names, so `{"user": {"email": …}}` is a
/// column called `user.email`. An array of strings becomes one value, comma
/// separated, which is what a list of tags looks like in a column.
pub fn parse_json(text: &str) -> Result<Table, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let objects = find_objects(&value).ok_or("no list of contacts in it")?;

    let mut headers: Vec<String> = Vec::new();
    let mut flat_rows: Vec<Vec<(String, String)>> = Vec::new();
    for object in objects {
        let Value::Object(map) = object else { continue };
        let mut flat = Vec::new();
        for (key, value) in map {
            match value {
                Value::Object(inner) => {
                    for (inner_key, inner_value) in inner {
                        if let Some(text) = scalar(inner_value) {
                            flat.push((format!("{key}.{inner_key}"), text));
                        }
                    }
                }
                other => {
                    if let Some(text) = scalar(other) {
                        flat.push((key.clone(), text));
                    }
                }
            }
        }
        for (key, _) in &flat {
            if !headers.contains(key) {
                headers.push(key.clone());
            }
        }
        flat_rows.push(flat);
    }

    let rows = flat_rows
        .into_iter()
        .map(|flat| {
            headers
                .iter()
                .map(|h| flat.iter().find(|(k, _)| k == h).map(|(_, v)| v.clone()).unwrap_or_default())
                .collect()
        })
        .collect();
    Ok(Table { headers, rows })
}

fn find_objects(value: &Value) -> Option<&Vec<Value>> {
    const KEYS: [&str; 10] =
        ["data", "users", "results", "items", "contacts", "signups", "records", "members", "customers", "people"];
    match value {
        Value::Array(items) if items.iter().any(Value::is_object) => Some(items),
        Value::Object(map) => KEYS
            .iter()
            .filter_map(|key| map.get(*key))
            .chain(map.values())
            .find_map(|inner| match inner {
                Value::Array(items) if items.iter().any(Value::is_object) => Some(items),
                Value::Object(_) => find_objects(inner),
                _ => None,
            }),
        _ => None,
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(scalar).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        Value::Null | Value::Object(_) => None,
    }
}

/// What each column probably means, from its name and then its contents.
///
/// The names are the ones exporters actually write — Google's `E-mail 1 -
/// Value`, Outlook's `E-mail Address`, a database's `user_email` — compared
/// with the punctuation and case taken out. A file whose address column is
/// called something nobody would guess is found by looking for the column
/// that is mostly addresses.
pub fn guess(table: &Table) -> Vec<Column> {
    let mut columns: Vec<Column> = table.headers.iter().map(|h| guess_header(h)).collect();

    // One address column: the first. A second column that looks like an
    // address is kept as a field, where it is still there to read.
    let mut seen_email = false;
    for column in &mut columns {
        if *column == Column::Email {
            if seen_email {
                *column = Column::Field;
            }
            seen_email = true;
        }
    }

    if !seen_email {
        let best = (0..table.headers.len())
            .map(|i| {
                let values = table.samples(i, 20);
                let hits = values.iter().filter(|v| looks_like_address(v)).count();
                (i, hits, values.len())
            })
            .filter(|(_, hits, total)| *total > 0 && hits * 2 > *total)
            .max_by_key(|(_, hits, _)| *hits);
        if let Some((i, _, _)) = best {
            columns[i] = Column::Email;
        }
    }

    // A full name and a first name both: the full one wins, and the parts
    // are kept as fields rather than glued on top of it.
    if columns.contains(&Column::Name) {
        for column in &mut columns {
            if matches!(column, Column::FirstName | Column::LastName) {
                *column = Column::Field;
            }
        }
    }
    columns
}

fn guess_header(header: &str) -> Column {
    let key: String = header.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    match key.as_str() {
        "" => Column::Skip,
        "email" | "emailaddress" | "mail" | "useremail" | "primaryemail" | "email1value" | "emailaddr"
        | "workemail" | "contactemail" | "customeremail" | "memberemail" => Column::Email,
        "name" | "fullname" | "displayname" | "contactname" | "username" | "customername" | "membername" => {
            Column::Name
        }
        "firstname" | "givenname" | "first" | "fname" => Column::FirstName,
        "lastname" | "surname" | "familyname" | "last" | "lname" => Column::LastName,
        "company" | "organization" | "organisation" | "org" | "companyname" | "organization1name" | "business"
        | "employer" => Column::Company,
        "id" | "userid" | "uid" | "externalid" | "customerid" | "memberid" | "contactid" | "uuid" => Column::Id,
        "tags" | "labels" | "groups" | "groupmembership" | "categories" | "segments" => Column::Tags,
        key if key.contains("email") && !key.contains("type") && !key.contains("verified") => Column::Email,
        _ => Column::Field,
    }
}

fn looks_like_address(value: &str) -> bool {
    let value = value.trim();
    match value.split_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.') && !value.contains(' '),
        None => false,
    }
}

/// The rows, read with the meanings given.
pub fn contacts(table: &Table, columns: &[Column]) -> Vec<Contact> {
    table
        .rows
        .iter()
        .map(|row| {
            let mut contact = Contact::default();
            let mut first = String::new();
            let mut last = String::new();
            for (i, value) in row.iter().enumerate() {
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                match columns.get(i).copied().unwrap_or(Column::Skip) {
                    Column::Email => {
                        // A cell can hold several, which Google writes as
                        // `a@x ::: b@y`: the first is the one to write to.
                        for addr in value
                            .split([',', ';', ' '])
                            .map(|v| v.trim().trim_matches(['<', '>']))
                            .filter(|v| looks_like_address(v))
                        {
                            match contact.email.is_empty() {
                                true => contact.email = addr.to_string(),
                                false => contact.other_emails.push(addr.to_string()),
                            }
                        }
                    }
                    Column::Name => contact.name = value.to_string(),
                    Column::FirstName => first = value.to_string(),
                    Column::LastName => last = value.to_string(),
                    Column::Company => contact.company = value.to_string(),
                    Column::Id => contact.external_id = value.to_string(),
                    Column::Tags => contact.tags.extend(
                        value
                            .split([',', ';'])
                            .flat_map(|t| t.split(":::"))
                            // Google prefixes its system groups with `* `.
                            .map(|t| t.trim().trim_start_matches("* ").trim())
                            .filter(|t| !t.is_empty() && *t != "myContacts")
                            .map(str::to_string),
                    ),
                    Column::Field => {
                        if let Some(header) = table.headers.get(i).filter(|h| !h.trim().is_empty()) {
                            contact.fields.push((header.clone(), value.to_string()));
                        }
                    }
                    Column::Skip => {}
                }
            }
            if contact.name.is_empty() {
                contact.name = format!("{first} {last}").trim().to_string();
            }
            contact
        })
        .collect()
}

/// Reads every card in a vCard file — what Contacts on a Mac, Google and
/// Outlook all export.
///
/// The properties a client book has a use for are read and the rest are left:
/// a photo encoded inline is most of the file's size and none of its point.
pub fn parse_vcard(text: &str) -> Vec<Contact> {
    // A long line is folded onto the next with a leading space or tab.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        match raw.strip_prefix([' ', '\t']) {
            Some(rest) if !lines.is_empty() => lines.last_mut().expect("checked").push_str(rest),
            _ => lines.push(raw.trim_end_matches('\r').to_string()),
        }
    }

    let mut cards = Vec::new();
    let mut card: Option<Contact> = None;
    let mut structured_name = String::new();

    for line in lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        // `item1.EMAIL;TYPE=work` → `EMAIL`. Apple groups properties with a
        // prefix, and the parameters say nothing a book needs.
        let name = head.split(';').next().unwrap_or(head);
        let name = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();

        match name.as_str() {
            "BEGIN" if value.eq_ignore_ascii_case("vcard") => {
                card = Some(Contact::default());
                structured_name.clear();
            }
            "END" if value.eq_ignore_ascii_case("vcard") => {
                if let Some(mut done) = card.take() {
                    if done.name.is_empty() {
                        done.name = structured_name.clone();
                    }
                    cards.push(done);
                }
            }
            _ => {
                let Some(current) = card.as_mut() else { continue };
                let value = unescape(value);
                match name.as_str() {
                    "FN" => current.name = value.trim().to_string(),
                    "N" => {
                        let parts: Vec<&str> = value.split(';').collect();
                        let last = parts.first().copied().unwrap_or("").trim();
                        let first = parts.get(1).copied().unwrap_or("").trim();
                        structured_name = format!("{first} {last}").trim().to_string();
                    }
                    "EMAIL" => {
                        let addr = value.trim().trim_start_matches("mailto:").to_string();
                        if !looks_like_address(&addr) {
                            continue;
                        }
                        match current.email.is_empty() {
                            true => current.email = addr,
                            false => current.other_emails.push(addr),
                        }
                    }
                    "ORG" => current.company = value.split(';').next().unwrap_or("").trim().to_string(),
                    "UID" => current.external_id = value.trim().to_string(),
                    "CATEGORIES" => current
                        .tags
                        .extend(value.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty())),
                    "TEL" => push_field(current, "Phone", &value),
                    "TITLE" => push_field(current, "Title", &value),
                    "NOTE" => push_field(current, "Note", &value),
                    "URL" => push_field(current, "Website", &value),
                    "ADR" => {
                        let address = value
                            .split(';')
                            .map(str::trim)
                            .filter(|p| !p.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ");
                        push_field(current, "Address", &address);
                    }
                    _ => {}
                }
            }
        }
    }
    cards
}

/// A field, or a second value appended to the first: two phone numbers are
/// one field reading `+1 555…, +44 20…` rather than one lost.
fn push_field(contact: &mut Contact, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match contact.fields.iter_mut().find(|(k, _)| k == key) {
        Some((_, existing)) => {
            existing.push_str(", ");
            existing.push_str(value);
        }
        None => contact.fields.push((key.to_string(), value.to_string())),
    }
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

/// Which format a file is, from its name and then its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    VCard,
    Json,
}

pub fn sniff(name: &str, text: &str) -> Format {
    let lower = name.to_lowercase();
    if lower.ends_with(".vcf") || lower.ends_with(".vcard") {
        return Format::VCard;
    }
    if lower.ends_with(".json") {
        return Format::Json;
    }
    let start = text.trim_start_matches('\u{feff}').trim_start();
    if start.len() >= 11 && start[..11].eq_ignore_ascii_case("begin:vcard") {
        Format::VCard
    } else if start.starts_with('[') || start.starts_with('{') {
        Format::Json
    } else {
        Format::Csv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_csv_with_quotes_commas_and_newlines_reads_as_written() {
        let table = parse_csv("\u{feff}Name,Email,Notes\r\n\"Ruiz, Ana\",ana@acme.com,\"said \"\"yes\"\"\nthen left\"\r\n\r\nBo,bo@acme.com,\n");
        assert_eq!(table.headers, ["Name", "Email", "Notes"]);
        assert_eq!(table.rows.len(), 2, "the blank line is not a row");
        assert_eq!(table.rows[0], ["Ruiz, Ana", "ana@acme.com", "said \"yes\"\nthen left"]);
        assert_eq!(table.rows[1], ["Bo", "bo@acme.com", ""]);
    }

    #[test]
    fn a_semicolon_file_is_read_with_semicolons() {
        let table = parse_csv("Name;E-mail\nAna;ana@acme.com\n");
        assert_eq!(table.rows[0], ["Ana", "ana@acme.com"]);
    }

    #[test]
    fn the_usual_exports_have_their_columns_recognised() {
        let google = parse_csv("First Name,Last Name,E-mail 1 - Value,Organization 1 - Name,Group Membership\nAna,Ruiz,ana@acme.com,Acme,* myContacts ::: Clients\n");
        let columns = guess(&google);
        assert_eq!(columns, [Column::FirstName, Column::LastName, Column::Email, Column::Company, Column::Tags]);

        let contacts = contacts(&google, &columns);
        assert_eq!(contacts[0].name, "Ana Ruiz");
        assert_eq!(contacts[0].email, "ana@acme.com");
        assert_eq!(contacts[0].company, "Acme");
        assert_eq!(contacts[0].tags, ["Clients"], "Google's own system group is not a tag");
    }

    #[test]
    fn an_address_column_nobody_could_guess_is_found_by_its_contents() {
        let table = parse_csv("who,contact,signed\nAna,ana@acme.com,2026-01-02\nBo,bo@acme.com,2026-01-03\n");
        let columns = guess(&table);
        assert_eq!(columns[1], Column::Email);
        let contacts = contacts(&table, &columns);
        assert_eq!(contacts[0].fields, [("who".to_string(), "Ana".to_string()), ("signed".to_string(), "2026-01-02".to_string())]);
    }

    #[test]
    fn a_feed_of_signups_reads_wherever_it_keeps_them() {
        let feed = r#"{"meta": {"count": 2}, "data": [
            {"id": 7, "email": "ana@acme.com", "name": "Ana", "plan": "pro", "tags": ["beta", "vip"], "profile": {"company": "Acme"}},
            {"id": 8, "email": "bo@acme.com", "verified": true}
        ]}"#;
        let table = parse_json(feed).unwrap();
        assert!(table.headers.contains(&"profile.company".to_string()));
        let mut columns = guess(&table);
        let company = table.headers.iter().position(|h| h == "profile.company").unwrap();
        columns[company] = Column::Company;

        let contacts = contacts(&table, &columns);
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].external_id, "7");
        assert_eq!(contacts[0].tags, ["beta", "vip"]);
        assert_eq!(contacts[0].company, "Acme");
        assert!(contacts[0].fields.contains(&("plan".to_string(), "pro".to_string())));
        assert_eq!(contacts[1].email, "bo@acme.com");
    }

    #[test]
    fn json_that_holds_no_contacts_says_so() {
        assert!(parse_json("{\"ok\": true}").is_err());
        assert!(parse_json("not json").is_err());
    }

    #[test]
    fn vcards_from_a_mac_read_with_their_groups_folds_and_escapes() {
        let file = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Ruiz;Ana;;;\r\nFN:Ana Ruiz\r\nORG:Acme\\, Inc.;Design\r\n\
                    item1.EMAIL;type=INTERNET;type=pref:ana@acme.com\r\nEMAIL;type=HOME:ana@home.example\r\n\
                    TEL;type=CELL:+1 555 0100\r\nNOTE:Met at the\r\n  spring show\r\nCATEGORIES:clients,vip\r\nEND:VCARD\r\n\
                    BEGIN:VCARD\r\nN:Chen;Bo;;;\r\nEMAIL:bo@acme.com\r\nEND:VCARD\r\n";
        let cards = parse_vcard(file);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "Ana Ruiz");
        assert_eq!(cards[0].company, "Acme, Inc.");
        assert_eq!(cards[0].email, "ana@acme.com");
        assert_eq!(cards[0].other_emails, ["ana@home.example"]);
        assert_eq!(cards[0].tags, ["clients", "vip"]);
        assert!(cards[0].fields.contains(&("Note".to_string(), "Met at the spring show".to_string())));
        assert_eq!(cards[1].name, "Bo Chen", "the structured name stands in for a missing FN");
    }

    #[test]
    fn a_file_says_what_it_is() {
        assert_eq!(sniff("export.vcf", ""), Format::VCard);
        assert_eq!(sniff("contacts", "BEGIN:VCARD\n"), Format::VCard);
        assert_eq!(sniff("users", " [{\"email\": \"a@b.c\"}]"), Format::Json);
        assert_eq!(sniff("users.csv", "email\n"), Format::Csv);
    }
}
