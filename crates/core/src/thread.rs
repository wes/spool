//! Conversation threading.
//!
//! A cut-down JWZ: messages are linked by `Message-ID` / `In-Reply-To` /
//! `References`, with a normalized-subject fallback for the very common case of
//! a client that replies without setting reply headers.
//!
//! The [`Threader`] is incremental — it is fed one message at a time in
//! whatever order they arrive and hands back the thread each belongs to,
//! merging two thread ids when a late-arriving message turns out to bridge
//! them. That matters because IMAP hands us messages newest-first, so a parent
//! routinely shows up after its children.

use crate::ThreadId;
use std::collections::HashMap;

/// Strips reply and forward prefixes so `Re: Re: Fwd: Launch` and `Launch`
/// land in the same conversation.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();

    loop {
        let lower = s.to_ascii_lowercase();
        let stripped = ["re:", "fwd:", "fw:", "aw:", "sv:", "vs:", "antw:"]
            .iter()
            .find_map(|p| lower.starts_with(p).then(|| s[p.len()..].trim_start()));

        // `Re[2]:` — Outlook and a few mailing list managers number their
        // replies rather than stacking prefixes.
        let stripped = stripped.or_else(|| {
            let rest = s.strip_prefix("Re[").or_else(|| s.strip_prefix("re["))?;
            let close = rest.find(']')?;
            rest[..close].parse::<u32>().ok()?;
            Some(rest[close + 1..].trim_start().strip_prefix(':')?.trim_start())
        });

        match stripped {
            Some(next) if next.len() < s.len() => s = next,
            _ => break,
        }
    }

    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Incrementally assigns messages to conversations.
#[derive(Default)]
pub struct Threader {
    /// `Message-ID` -> thread. Also holds ids seen only as a reference, so a
    /// parent that has not arrived yet still anchors its children together.
    by_message_id: HashMap<String, ThreadId>,
    by_subject: HashMap<String, ThreadId>,
    /// Union-find parent pointers, so merging two threads stays O(~1) and we
    /// never have to rewrite ids we already handed out.
    parent: HashMap<i64, i64>,
    next_id: i64,
}

/// The headers threading needs. Borrowed, so callers can feed parsed messages
/// without allocating.
pub struct ThreadKeys<'a> {
    pub message_id: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [&'a str],
    pub subject: &'a str,
}

impl Threader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the threader from messages already in the store, so a second run
    /// against the same database keeps existing conversations intact.
    pub fn seed(&mut self, message_id: &str, thread: ThreadId) {
        self.parent.entry(thread.0).or_insert(thread.0);
        self.next_id = self.next_id.max(thread.0 + 1);
        if !message_id.is_empty() {
            self.by_message_id.insert(message_id.to_string(), thread);
        }
    }

    pub fn seed_subject(&mut self, normalized: &str, thread: ThreadId) {
        if !normalized.is_empty() {
            self.by_subject.entry(normalized.to_string()).or_insert(thread);
        }
    }

    /// Returns the conversation this message belongs to, creating or merging
    /// threads as needed.
    pub fn assign(&mut self, keys: ThreadKeys<'_>) -> ThreadId {
        // Every id this message links to should end up in one thread.
        let mut linked: Vec<&str> = Vec::with_capacity(keys.references.len() + 1);
        linked.extend(keys.in_reply_to.filter(|s| !s.is_empty()));
        linked.extend(keys.references.iter().copied().filter(|s| !s.is_empty()));

        let known: Vec<ThreadId> = linked
            .iter()
            .filter_map(|id| self.by_message_id.get(*id).copied())
            .collect();
        let mut thread = known.into_iter().reduce(|a, b| self.union(a, b));

        // A message we have already seen — as a real message or as a dangling
        // reference from one of its children — keeps its thread.
        if let Some(own) = keys.message_id.filter(|s| !s.is_empty())
            && let Some(existing) = self.by_message_id.get(own).copied()
        {
            thread = Some(match thread {
                Some(t) => self.union(t, existing),
                None => existing,
            });
        }

        let normalized = normalize_subject(keys.subject);

        // Subject fallback, but only when the message carries no reply headers
        // at all. Applying it more eagerly is how mail clients end up merging
        // every message titled "hello" into one giant thread.
        if thread.is_none() && linked.is_empty() && !normalized.is_empty() {
            thread = self.by_subject.get(&normalized).copied();
        }

        let thread = match thread {
            Some(t) => self.find(t),
            None => self.fresh(),
        };

        if !normalized.is_empty() {
            self.by_subject.entry(normalized).or_insert(thread);
        }
        for id in linked {
            self.by_message_id.entry(id.to_string()).or_insert(thread);
        }
        if let Some(own) = keys.message_id.filter(|s| !s.is_empty()) {
            self.by_message_id.insert(own.to_string(), thread);
        }

        thread
    }

    fn fresh(&mut self) -> ThreadId {
        let id = self.next_id;
        self.next_id += 1;
        self.parent.insert(id, id);
        ThreadId(id)
    }

    /// Union-find root, with path compression.
    fn find(&mut self, thread: ThreadId) -> ThreadId {
        let mut root = thread.0;
        while let Some(&up) = self.parent.get(&root)
            && up != root
        {
            root = up;
        }
        let mut cursor = thread.0;
        while let Some(&up) = self.parent.get(&cursor)
            && up != cursor
        {
            self.parent.insert(cursor, root);
            cursor = up;
        }
        ThreadId(root)
    }

    fn union(&mut self, a: ThreadId, b: ThreadId) -> ThreadId {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return ra;
        }
        // Keep the lower id as the root: it is the older conversation, which
        // makes thread ids stable across re-syncs.
        let (keep, drop) = if ra.0 <= rb.0 { (ra, rb) } else { (rb, ra) };
        self.parent.insert(drop.0, keep.0);
        keep
    }

    /// Resolves a thread id handed out earlier to its current root. Callers
    /// that retained ids across a merge need this before writing them back.
    pub fn resolve(&mut self, thread: ThreadId) -> ThreadId {
        self.find(thread)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<'a>(
        message_id: &'a str,
        in_reply_to: Option<&'a str>,
        references: &'a [&'a str],
        subject: &'a str,
    ) -> ThreadKeys<'a> {
        ThreadKeys { message_id: Some(message_id), in_reply_to, references, subject }
    }

    #[test]
    fn reply_prefixes_are_stripped() {
        assert_eq!(normalize_subject("Re: Re: Fwd: Launch plan"), "launch plan");
        assert_eq!(normalize_subject("RE: Launch  plan "), "launch plan");
        assert_eq!(normalize_subject("Re[2]: Launch plan"), "launch plan");
        assert_eq!(normalize_subject("Regarding launch"), "regarding launch");
    }

    #[test]
    fn replies_join_their_parent() {
        let mut t = Threader::new();
        let a = t.assign(keys("<a>", None, &[], "Launch plan"));
        let b = t.assign(keys("<b>", Some("<a>"), &["<a>"], "Re: Launch plan"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_parent_arriving_late_merges_its_children() {
        let mut t = Threader::new();
        // Children first, as IMAP delivers them, each referencing an unseen parent.
        let b = t.assign(keys("<b>", Some("<a>"), &["<a>"], "Re: Launch"));
        let c = t.assign(keys("<c>", Some("<a>"), &["<a>"], "Re: Launch"));
        assert_eq!(b, c, "siblings share a thread via their common reference");

        let a = t.assign(keys("<a>", None, &[], "Launch"));
        assert_eq!(t.resolve(a), t.resolve(b));
    }

    #[test]
    fn two_threads_merge_when_a_message_bridges_them() {
        let mut t = Threader::new();
        let a = t.assign(keys("<a>", None, &[], "Alpha"));
        let b = t.assign(keys("<b>", None, &[], "Beta"));
        assert_ne!(a, b);

        // A message referencing both roots pulls the conversations together.
        let c = t.assign(keys("<c>", Some("<a>"), &["<a>", "<b>"], "Re: Alpha"));
        assert_eq!(t.resolve(a), t.resolve(b));
        assert_eq!(t.resolve(c), t.resolve(a));
    }

    #[test]
    fn subject_fallback_only_applies_without_reply_headers() {
        let mut t = Threader::new();
        let a = t.assign(keys("<a>", None, &[], "Standup notes"));
        let b = t.assign(keys("<b>", None, &[], "Re: Standup notes"));
        assert_eq!(a, b, "a bare reply still finds its thread by subject");

        // This one names a parent we have never seen, so it is its own thread
        // rather than being swept into the subject bucket.
        let c = t.assign(keys("<c>", Some("<zz>"), &["<zz>"], "Re: Standup notes"));
        assert_ne!(t.resolve(c), t.resolve(a));
    }

    #[test]
    fn unrelated_messages_stay_apart() {
        let mut t = Threader::new();
        let a = t.assign(keys("<a>", None, &[], "Invoice 41"));
        let b = t.assign(keys("<b>", None, &[], "Invoice 42"));
        assert_ne!(a, b);
    }

    #[test]
    fn seeding_preserves_ids_across_restarts() {
        let mut t = Threader::new();
        t.seed("<a>", ThreadId(7));
        let b = t.assign(keys("<b>", Some("<a>"), &["<a>"], "Re: Launch"));
        assert_eq!(b, ThreadId(7));
        // Fresh threads must not collide with the seeded id.
        assert_ne!(t.assign(keys("<c>", None, &[], "Other")), ThreadId(7));
    }
}
