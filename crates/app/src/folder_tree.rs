//! The sidebar's folder hierarchy.
//!
//! IMAP has no tree: it has a flat list of paths and one delimiter character
//! that pretends otherwise. `[Gmail]/All Mail`, `INBOX.Lists.rust` — the
//! nesting is a naming convention, and turning it back into a shape the
//! sidebar can indent and collapse is this module's whole job.
//!
//! Kept out of the render path deliberately: this is the only judgement in the
//! sidebar, and everything around it is layout. Building it here means it can
//! be tested without a window.

use spool_core::{Mailbox, MailboxId, MailboxRole};

/// One row of the folder tree.
pub struct Node<'a> {
    /// The server path this node stands for — the identity used to remember
    /// that it was collapsed.
    pub path: String,
    /// What the row is labelled with: the last segment of the path.
    pub name: String,
    /// The folder itself, when the server has one at this path.
    ///
    /// `None` for a container that exists only as a prefix of its children:
    /// Gmail's `[Gmail]` is `\Noselect` and never comes back from `LIST` as
    /// something you can open, but four folders live under it and they have to
    /// hang off something.
    pub mailbox: Option<&'a Mailbox>,
    pub children: Vec<Node<'a>>,
}

impl<'a> Node<'a> {
    pub fn id(&self) -> Option<MailboxId> {
        self.mailbox.map(|m| m.id)
    }

    pub fn role(&self) -> MailboxRole {
        self.mailbox.map_or(MailboxRole::Other, |m| m.role)
    }

    /// Unread in this folder and everything under it.
    ///
    /// For a container only — see [`Node::counts`]. A folder with mail of its
    /// own reports that, collapsed or not: rolling its children in makes the
    /// number beside `INBOX` mean something different depending on a twisty,
    /// and the one it means when closed is not the count of unread mail in
    /// the inbox.
    pub fn unread(&self) -> u32 {
        self.mailbox.map_or(0, |m| m.unread)
            + self.children.iter().map(Node::unread).sum::<u32>()
    }

    /// Messages in this folder and everything under it.
    pub fn total(&self) -> u32 {
        self.mailbox.map_or(0, |m| m.total) + self.children.iter().map(Node::total).sum::<u32>()
    }

    /// The unread and total a row should show, given whether it is folded.
    ///
    /// A real folder speaks for itself and nothing else, however its children
    /// are arranged — that is the whole point of the number, and a count that
    /// changes when you fold the row is a count you cannot trust.
    ///
    /// A container is the exception, because it is not a folder: `[Gmail]` is
    /// `\Noselect` and holds no mail, so its own count is always zero and the
    /// roll-up is the only thing it could honestly say. Open, it says nothing
    /// and lets its children speak.
    pub fn counts(&self, folded: bool) -> (u32, u32) {
        match (self.mailbox, folded) {
            (Some(mailbox), _) => (mailbox.unread, mailbox.total),
            (None, true) => (self.unread(), self.total()),
            (None, false) => (0, 0),
        }
    }

    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Groups one account's folders into a tree, keeping the order they arrive in.
///
/// The store hands folders back sorted by role and then by path, which is the
/// order the sidebar wants; a parent therefore takes the position of its first
/// child, and siblings keep their relative order.
pub fn build<'a>(folders: &[&'a Mailbox]) -> Vec<Node<'a>> {
    let delimiter = delimiter_of(folders);
    let mut roots: Vec<Node<'a>> = Vec::new();

    for folder in folders {
        let mut level = &mut roots;
        let mut prefix = String::new();

        let mut segments = folder.path.split(delimiter).peekable();
        while let Some(segment) = segments.next() {
            if !prefix.is_empty() {
                prefix.push(delimiter);
            }
            prefix.push_str(segment);

            let at = match level.iter().position(|node| node.path == prefix) {
                Some(at) => at,
                None => {
                    level.push(Node {
                        path: prefix.clone(),
                        name: segment.to_string(),
                        mailbox: None,
                        children: Vec::new(),
                    });
                    level.len() - 1
                }
            };

            // The last segment is the folder itself; the ones before it are
            // containers, which may or may not turn out to be real folders of
            // their own later in the list.
            if segments.peek().is_none() {
                level[at].mailbox = Some(folder);
            }
            level = &mut level[at].children;
        }
    }

    roots
}

/// Groups one account's folders into a tree and puts every level in order.
///
/// The inbox first if there is one, then by name, ignoring case — the same
/// rule at every depth. Nothing here is arranged by hand: what the reader
/// arranges is the order of the accounts, and a folder tree that also had its
/// own arrangement would be a sidebar with two kinds of order in it.
pub fn build_sorted<'a>(folders: &[&'a Mailbox]) -> Vec<Node<'a>> {
    let mut roots = build(folders);
    sort_level(&mut roots);
    roots
}

/// The rule, applied to a level and everything under it.
fn sort_level(nodes: &mut [Node<'_>]) {
    for node in nodes.iter_mut() {
        sort_level(&mut node.children);
    }

    // The inbox is not a folder like the others: it is where mail arrives,
    // and a sidebar that files it under I between `Archive` and `Lists` is
    // one that has sorted the point away.
    nodes.sort_by(|a, b| {
        let rank = |node: &Node<'_>| u8::from(node.role() != MailboxRole::Inbox);
        rank(a).cmp(&rank(b)).then_with(|| folded(&a.name).cmp(&folded(&b.name)))
    });
}

/// What a name sorts as.
///
/// Lowercased, so `archive` and `Archive` are not two groups; the server picks
/// the casing and it is nothing the reader asked for.
fn folded(name: &str) -> String {
    name.to_lowercase()
}

/// The character this account separates path segments with./// The character this account separates path segments with.
///
/// IMAP servers each pick their own and announce it in `LIST`, which the
/// sidebar does not have to hand — but only two characters are in real use and
/// the answer is legible from the paths themselves.
///
/// `/` is the assumption, and `.` has to be earned: Dovecot's dotted layout
/// hangs everything off the inbox, so `INBOX.Lists` is the evidence that a dot
/// separates rather than belongs to a name. Without it, a flat account with a
/// folder called `example.com` would be split into a folder called `com` that
/// nobody has — inventing a row is a worse failure than leaving one flat.
fn delimiter_of(folders: &[&Mailbox]) -> char {
    if folders.iter().any(|m| m.path.contains('/')) {
        return '/';
    }
    match folders.iter().any(|m| {
        m.path.len() > 6 && m.path[..6].eq_ignore_ascii_case("INBOX.")
    }) {
        true => '.',
        false => '/',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_core::AccountId;

    fn mailbox(path: &str, unread: u32) -> Mailbox {
        Mailbox {
            id: MailboxId(path.len() as i64),
            account_id: AccountId(1),
            path: path.to_string(),
            name: path.rsplit(['/', '.']).next().unwrap_or(path).to_string(),
            role: MailboxRole::guess(path),
            total: 10,
            unread,
            server_total: 0,
        }
    }

    /// The rows as drawn.
    fn tree(paths: &[(&str, u32)]) -> Vec<String> {
        let boxes: Vec<Mailbox> = paths.iter().map(|(p, u)| mailbox(p, *u)).collect();
        let refs: Vec<&Mailbox> = boxes.iter().collect();
        let mut out = Vec::new();
        fn walk(nodes: &[Node], depth: usize, out: &mut Vec<String>) {
            for node in nodes {
                out.push(format!(
                    "{}{}{}",
                    "  ".repeat(depth),
                    node.name,
                    if node.mailbox.is_some() { "" } else { " (container)" },
                ));
                walk(&node.children, depth + 1, out);
            }
        }
        walk(&build_sorted(&refs), 0, &mut out);
        out
    }


    /// The paths one account has, in the order the store hands them back:
    /// role first, then path.
    const ACCOUNT: [(&str, u32); 5] =
        [("INBOX", 4), ("Sent", 0), ("Archive", 0), ("betty", 9), ("Alerts", 2)];

    #[test]
    fn folders_are_alphabetical_with_the_inbox_kept_at_the_top() {
        // `betty` sorts among the capitals rather than after them, and the
        // inbox stays where mail arrives rather than filing itself under I.
        assert_eq!(
            tree(&ACCOUNT),
            ["INBOX", "Alerts", "Archive", "betty", "Sent"],
        );
    }

    #[test]
    fn the_rule_reaches_every_level_of_the_tree() {
        assert_eq!(
            tree(&[("INBOX", 0), ("Lists/zed", 0), ("Lists/ansible", 0), ("Lists/rust", 0)]),
            ["INBOX", "Lists (container)", "  ansible", "  rust", "  zed"],
        );
    }

    #[test]
    fn gmails_noselect_container_still_gets_a_row_to_hang_its_folders_from() {
        assert_eq!(
            tree(&[("INBOX", 0), ("[Gmail]/All Mail", 0), ("[Gmail]/Trash", 0)]),
            ["INBOX", "[Gmail] (container)", "  All Mail", "  Trash"],
        );
    }

    #[test]
    fn a_dovecot_account_nests_on_its_own_delimiter() {
        assert_eq!(
            tree(&[("INBOX", 0), ("INBOX.Lists", 0), ("INBOX.Lists.rust", 0)]),
            ["INBOX", "  Lists", "    rust"],
        );
    }

    #[test]
    fn a_dot_inside_a_name_is_not_a_delimiter_where_the_server_uses_slashes() {
        // A Gmail label really can be called this, and splitting it would put
        // a folder called `com` in the sidebar.
        assert_eq!(
            tree(&[("INBOX", 0), ("example.com", 0), ("[Gmail]/Trash", 0)]),
            // Alphabetical below the inbox, and a bracket sorts before a
            // letter — the point here is the row count, not the order.
            ["INBOX", "[Gmail] (container)", "  Trash", "example.com"],
        );

        // And the same account with nothing nested at all, where there is no
        // slash to go on either: still not a folder called `com`.
        assert_eq!(tree(&[("INBOX", 0), ("example.com", 0)]), ["INBOX", "example.com"]);
    }

    #[test]
    fn a_parent_counts_what_is_hidden_underneath_it() {
        let boxes: Vec<Mailbox> =
            [("Lists", 1), ("Lists/rust", 3), ("Lists/rust/internals", 5)]
                .iter()
                .map(|(p, u)| mailbox(p, *u))
                .collect();
        let refs: Vec<&Mailbox> = boxes.iter().collect();
        let roots = build(&refs);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].unread(), 9, "the roll-up reaches every descendant");
        assert_eq!(roots[0].total(), 30);
        assert!(roots[0].has_children());
    }

    #[test]
    fn a_folder_with_mail_of_its_own_counts_only_that_mail() {
        let boxes: Vec<Mailbox> = [("INBOX", 1), ("INBOX/Lists", 3), ("INBOX/Lists/rust", 5)]
            .iter()
            .map(|(p, u)| mailbox(p, *u))
            .collect();
        let refs: Vec<&Mailbox> = boxes.iter().collect();
        let roots = build(&refs);

        // The number beside INBOX is the same whether or not its children are
        // showing, and it is the inbox's own unread rather than 9.
        assert_eq!(roots[0].counts(true).0, 1);
        assert_eq!(roots[0].counts(false).0, 1);
    }

    #[test]
    fn a_container_falls_back_to_what_it_is_hiding() {
        // `[Gmail]` never comes back from LIST, so the tree builds it as a
        // node with no mailbox — the one case with no count of its own.
        let boxes: Vec<Mailbox> =
            [("[Gmail]/All Mail", 4), ("[Gmail]/Spam", 2)].iter().map(|(p, u)| mailbox(p, *u)).collect();
        let refs: Vec<&Mailbox> = boxes.iter().collect();
        let roots = build(&refs);

        assert!(roots[0].mailbox.is_none(), "a Noselect prefix is a container");
        assert_eq!(roots[0].counts(true).0, 6, "folded, it speaks for its children");
        assert_eq!(roots[0].counts(false).0, 0, "open, they speak for themselves");
    }

    #[test]
    fn a_folder_that_arrives_before_its_parent_does_not_get_two_rows() {
        // The store sorts by role first, so `INBOX.Archive` can be listed
        // ahead of the `INBOX` it belongs under.
        assert_eq!(
            tree(&[("INBOX.Archive", 0), ("INBOX", 0)]),
            ["INBOX", "  Archive"],
            "the container became the real folder rather than a second row",
        );
    }
}
