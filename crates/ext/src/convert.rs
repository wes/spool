//! Translation across the boundary.
//!
//! Every generated type is confined to this module. The app above sees
//! [`crate::api`] types and the store below sees its own, and neither learns
//! that WebAssembly is in the middle — which is the property that would let the
//! runtime be swapped without a single change outside this crate.
//!
//! Translation in the guest-to-host direction is also where policy is applied,
//! because that is the direction untrusted data travels. An effect the
//! extension was not granted is dropped here rather than rejected later; a
//! panel tree is validated here rather than trusted by the renderer; a URL is
//! reduced to a host here rather than pattern-matched as a string somewhere it
//! could be fooled.

use spool_core::{Account, Address, Attachment, Body, Envelope, Flags, Mailbox, MailboxRole};
use spool_store::{Query as StoreQuery, Scope};

use crate::{
    api,
    capability::Grants,
    services::{HttpRequest, HttpResponse, MAX_QUERY, MailQuery},
};

/// The generated types, under one short name.
pub(crate) mod wit {
    pub(crate) use crate::bindings::spool::extension::{effects, host, types, ui};
    pub(crate) use crate::bindings::{CommandContext, Decorations};
}

// ---- host to guest ----------------------------------------------------------

pub(crate) fn flags_to_wit(flags: Flags) -> wit::types::MessageFlags {
    let mut out = wit::types::MessageFlags::empty();
    for (bit, mapped) in FLAG_PAIRS {
        if flags.has(bit) {
            out |= mapped;
        }
    }
    out
}

pub(crate) fn flags_from_wit(flags: wit::types::MessageFlags) -> Flags {
    let mut out = Flags::default();
    for (bit, mapped) in FLAG_PAIRS {
        if flags.contains(mapped) {
            out = out.with(bit, true);
        }
    }
    out
}

/// The one place the two spellings of a flag are tied together.
///
/// A table rather than two `match` arms, because a flag added to one direction
/// and forgotten in the other is a bug that shows up as an extension quietly
/// unable to see something.
const FLAG_PAIRS: [(u32, wit::types::MessageFlags); 7] = [
    (Flags::SEEN, wit::types::MessageFlags::SEEN),
    (Flags::FLAGGED, wit::types::MessageFlags::FLAGGED),
    (Flags::ANSWERED, wit::types::MessageFlags::ANSWERED),
    (Flags::DRAFT, wit::types::MessageFlags::DRAFT),
    (Flags::DELETED, wit::types::MessageFlags::DELETED),
    (Flags::TRIAGED, wit::types::MessageFlags::TRIAGED),
    (Flags::FORWARDED, wit::types::MessageFlags::FORWARDED),
];

fn address_to_wit(address: &Address) -> wit::types::Address {
    wit::types::Address { name: address.name.clone(), addr: address.addr.clone() }
}

pub(crate) fn envelope_to_wit(envelope: &Envelope) -> wit::types::Envelope {
    wit::types::Envelope {
        id: envelope.id.0 as u64,
        account: envelope.account_id.0 as u64,
        mailbox: envelope.mailbox_id.0 as u64,
        thread: envelope.thread_id.0 as u64,
        from: address_to_wit(&envelope.from),
        subject: envelope.subject.clone(),
        date: envelope.date,
        snippet: envelope.snippet.clone(),
        flags: flags_to_wit(envelope.flags),
        has_attachments: envelope.has_attachments,
        size: envelope.size,
    }
}

pub(crate) fn body_to_wit(body: &Body) -> wit::types::Body {
    wit::types::Body {
        plain: body.plain.clone(),
        html: body.html.clone(),
        to: body.to.iter().map(address_to_wit).collect(),
        cc: body.cc.iter().map(address_to_wit).collect(),
        reply_to: body.reply_to.iter().map(address_to_wit).collect(),
        rfc_message_id: body.message_id.clone(),
        attachments: body.attachments.iter().map(attachment_to_wit).collect(),
    }
}

fn attachment_to_wit(attachment: &Attachment) -> wit::types::Attachment {
    wit::types::Attachment {
        name: attachment.name.clone(),
        mime: attachment.mime.clone(),
        size: attachment.size,
    }
}

pub(crate) fn mailbox_to_wit(mailbox: &Mailbox) -> wit::types::Mailbox {
    wit::types::Mailbox {
        id: mailbox.id.0 as u64,
        account: mailbox.account_id.0 as u64,
        path: mailbox.path.clone(),
        name: mailbox.name.clone(),
        role: match mailbox.role {
            MailboxRole::Inbox => wit::types::MailboxRole::Inbox,
            MailboxRole::Sent => wit::types::MailboxRole::Sent,
            MailboxRole::Drafts => wit::types::MailboxRole::Drafts,
            MailboxRole::Archive => wit::types::MailboxRole::Archive,
            MailboxRole::Trash => wit::types::MailboxRole::Trash,
            MailboxRole::Junk => wit::types::MailboxRole::Junk,
            MailboxRole::Other => wit::types::MailboxRole::Other,
        },
        total: mailbox.total,
        unread: mailbox.unread,
    }
}

pub(crate) fn account_to_wit(account: &Account) -> wit::types::Account {
    wit::types::Account {
        id: account.id.0 as u64,
        name: account.name.clone(),
        email: account.email.clone(),
    }
}

pub(crate) fn response_to_wit(response: HttpResponse) -> wit::host::Response {
    wit::host::Response {
        status: response.status,
        headers: response.headers,
        body: response.body,
    }
}

// ---- guest to host ----------------------------------------------------------

pub(crate) fn query_from_wit(query: wit::types::Query) -> MailQuery {
    MailQuery {
        scope: match query.scope {
            wit::types::Scope::Everything => Scope::Everything,
            wit::types::Scope::Account(id) => Scope::Account(spool_core::AccountId(id as i64)),
            wit::types::Scope::Mailbox(id) => Scope::Mailbox(spool_core::MailboxId(id as i64)),
        },
        filter: match query.filter {
            wit::types::Filter::All => StoreQuery::All,
            wit::types::Filter::Unread => StoreQuery::Unread,
            wit::types::Filter::Flagged => StoreQuery::Flagged,
            wit::types::Filter::Search(text) => StoreQuery::Text(text),
        },
        // Clamped rather than refused: an extension asking for everything
        // wants as much as it can have, and an error would only teach it to
        // ask for exactly the cap it cannot see.
        limit: (query.limit as usize).min(MAX_QUERY),
    }
}

pub(crate) fn request_from_wit(request: wit::host::Request) -> HttpRequest {
    HttpRequest {
        method: request.method,
        url: request.url,
        headers: request.headers,
        body: request.body,
    }
}

/// What came back from a call that returns effects.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub effects: Vec<api::Effect>,
    /// Effects asking for something the extension never declared.
    ///
    /// Counted rather than silently discarded: an extension that keeps trying
    /// to file mail without `modify-mail` is one whose manifest is wrong, and
    /// the log line that says so is the only way its author finds out.
    pub refused: usize,
    /// Effects past [`api::MAX_EFFECTS`].
    pub dropped: usize,
}

/// Turns what an extension returned into what the host will actually do.
pub(crate) fn effects_from_wit(grants: &Grants, raw: Vec<wit::effects::Effect>) -> Outcome {
    let mut outcome = Outcome::default();

    for effect in raw {
        if outcome.effects.len() >= api::MAX_EFFECTS {
            outcome.dropped += 1;
            continue;
        }

        let needed_granted = match &effect {
            wit::effects::Effect::SetFlags(_)
            | wit::effects::Effect::MoveTo(_)
            | wit::effects::Effect::Archive(_)
            | wit::effects::Effect::Trash(_) => grants.modifies_mail(),
            wit::effects::Effect::Notify(_) => grants.notifies(),
            wit::effects::Effect::Status(_)
            | wit::effects::Effect::OpenUrl(_)
            | wit::effects::Effect::InvalidatePanel(_) => grants.draws(),
        };
        if !needed_granted {
            outcome.refused += 1;
            continue;
        }

        outcome.effects.push(match effect {
            wit::effects::Effect::SetFlags(change) => api::Effect::SetFlags {
                message: message_id(change.message),
                add: flags_from_wit(change.add),
                // A flag in both lists comes off. "Remove" is the more
                // conservative of two contradictory instructions, and an
                // extension that sends both is not one to guess for.
                remove: flags_from_wit(change.remove),
            },
            wit::effects::Effect::MoveTo(move_to) => api::Effect::MoveTo {
                message: message_id(move_to.message),
                mailbox: spool_core::MailboxId(move_to.mailbox as i64),
            },
            wit::effects::Effect::Archive(id) => api::Effect::Archive(message_id(id)),
            wit::effects::Effect::Trash(id) => api::Effect::Trash(message_id(id)),
            wit::effects::Effect::Notify(note) => api::Effect::Notify {
                title: api::clamp(note.title),
                body: api::clamp(note.body),
                message: note.message.map(message_id),
            },
            wit::effects::Effect::Status(text) => api::Effect::Status(api::clamp(text)),
            wit::effects::Effect::OpenUrl(url) => api::Effect::OpenUrl(api::clamp(url)),
            wit::effects::Effect::InvalidatePanel(id) => {
                api::Effect::InvalidatePanel(api::clamp(id))
            }
        });
    }

    outcome
}

fn message_id(id: u64) -> spool_core::MessageId {
    spool_core::MessageId(id as i64)
}

pub(crate) fn decorations_from_wit(raw: wit::Decorations) -> api::Decorations {
    api::Decorations {
        banners: raw
            .banners
            .into_iter()
            .map(|banner| api::Banner {
                badges: banner.badges.into_iter().map(badge_from_wit).collect(),
                text: api::clamp(banner.text),
                emphasis: emphasis_from_wit(banner.emphasis),
                action: banner.action.map(button_from_wit),
            })
            .collect(),
        badges: raw.badges.into_iter().map(badge_from_wit).collect(),
    }
}

fn badge_from_wit(badge: wit::ui::Badge) -> api::Badge {
    api::Badge { label: api::clamp(badge.label), emphasis: emphasis_from_wit(badge.emphasis) }
}

pub(crate) fn panel_from_wit(tree: wit::ui::Tree) -> Result<api::Panel, api::TreeError> {
    let nodes = tree
        .nodes
        .into_iter()
        .map(|node| api::Node {
            kind: match node.kind {
                wit::ui::NodeKind::Row => api::NodeKind::Row,
                wit::ui::NodeKind::Column => api::NodeKind::Column,
                wit::ui::NodeKind::Text(text) => api::NodeKind::Text {
                    content: api::clamp(text.content),
                    emphasis: emphasis_from_wit(text.emphasis),
                },
                wit::ui::NodeKind::Button(button) => api::NodeKind::Button(button_from_wit(button)),
                wit::ui::NodeKind::Divider => api::NodeKind::Divider,
                wit::ui::NodeKind::Spacer => api::NodeKind::Spacer,
            },
            children: node.children,
        })
        .collect();

    api::Panel::validate(nodes, tree.root as usize)
}

fn button_from_wit(button: wit::ui::Button) -> api::Button {
    api::Button {
        label: api::clamp(button.label),
        run: api::clamp(button.run),
        enabled: button.enabled,
    }
}

fn emphasis_from_wit(emphasis: wit::ui::Emphasis) -> api::Emphasis {
    match emphasis {
        wit::ui::Emphasis::Normal => api::Emphasis::Normal,
        wit::ui::Emphasis::Muted => api::Emphasis::Muted,
        wit::ui::Emphasis::Strong => api::Emphasis::Strong,
        wit::ui::Emphasis::Heading => api::Emphasis::Heading,
        wit::ui::Emphasis::Accent => api::Emphasis::Accent,
        wit::ui::Emphasis::Danger => api::Emphasis::Danger,
    }
}

/// The host a URL actually addresses.
///
/// Hand-rolled rather than a URL crate, because the only question being asked
/// is which host a request will reach, and the answer has to be the one the
/// network stack will arrive at — not the one a pattern match on a string
/// finds first.
///
/// The case that makes this worth its own function is userinfo.
/// `https://api.github.com@evil.example/` is a request to `evil.example`, and
/// anything comparing the text after `https://` against a declared host would
/// happily approve it.
pub(crate) fn url_host(url: &str) -> Option<&str> {
    // HTTPS only, checked here as well as in the capability, since this is
    // what the request actually goes through.
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("HTTPS://"))?;

    // The authority runs to the first delimiter of anything that follows it.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);

    // Userinfo is everything before the *last* `@`: a name may contain one,
    // and the host is what comes after all of them.
    let host_port = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };

    // A port, but not the colons inside an IPv6 literal.
    let host = match host_port.starts_with('[') {
        true => host_port.split(']').next().map(|h| &h[1..]).unwrap_or(host_port),
        false => host_port.split(':').next().unwrap_or(host_port),
    };

    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    fn grants(caps: &[&str]) -> Grants {
        Grants::new(caps.iter().map(|c| c.parse::<Capability>().unwrap()))
    }

    #[test]
    fn every_flag_survives_the_round_trip() {
        // The table is the only thing tying the two spellings together, and a
        // flag missing from it is an extension that cannot see or set one.
        for (bit, _) in FLAG_PAIRS {
            let flags = Flags::default().with(bit, true);
            assert_eq!(flags_from_wit(flags_to_wit(flags)), flags, "bit {bit}");
        }

        let all = FLAG_PAIRS.iter().fold(Flags::default(), |f, (bit, _)| f.with(*bit, true));
        assert_eq!(flags_from_wit(flags_to_wit(all)), all);
        assert_eq!(flags_from_wit(flags_to_wit(Flags::default())), Flags::default());
    }

    #[test]
    fn an_effect_the_extension_did_not_ask_for_is_refused() {
        let raw = vec![
            wit::effects::Effect::Trash(7),
            wit::effects::Effect::Status("hello".into()),
        ];

        // `ui` but not `modify-mail`: the status line lands, the deletion does
        // not.
        let outcome = effects_from_wit(&grants(&["ui"]), raw);
        assert_eq!(outcome.refused, 1);
        assert_eq!(outcome.effects, vec![api::Effect::Status("hello".into())]);
    }

    #[test]
    fn an_extension_with_every_grant_gets_every_effect() {
        let raw = vec![
            wit::effects::Effect::Archive(1),
            wit::effects::Effect::Trash(2),
            wit::effects::Effect::Status("s".into()),
            wit::effects::Effect::Notify(wit::effects::Notification {
                title: "t".into(),
                body: "b".into(),
                message: Some(3),
            }),
        ];

        let outcome = effects_from_wit(&grants(&["modify-mail", "ui", "notify"]), raw);
        assert_eq!(outcome.refused, 0);
        assert_eq!(outcome.effects.len(), 4);
    }

    #[test]
    fn a_flood_of_effects_is_cut_off_rather_than_applied() {
        let raw: Vec<_> = (0..api::MAX_EFFECTS + 50)
            .map(|i| wit::effects::Effect::Archive(i as u64))
            .collect();

        let outcome = effects_from_wit(&grants(&["modify-mail"]), raw);
        assert_eq!(outcome.effects.len(), api::MAX_EFFECTS);
        assert_eq!(outcome.dropped, 50);
    }

    #[test]
    fn a_query_is_clamped_rather_than_taken_at_its_word() {
        let query = wit::types::Query {
            scope: wit::types::Scope::Everything,
            filter: wit::types::Filter::Search("from:ana".into()),
            limit: u32::MAX,
        };
        let converted = query_from_wit(query);
        assert_eq!(converted.limit, MAX_QUERY);
        assert_eq!(converted.filter, StoreQuery::Text("from:ana".into()));
        assert_eq!(converted.scope, Scope::Everything);
    }

    #[test]
    fn a_url_reduces_to_the_host_it_will_actually_reach() {
        assert_eq!(url_host("https://api.github.com/repos"), Some("api.github.com"));
        assert_eq!(url_host("https://api.github.com"), Some("api.github.com"));
        assert_eq!(url_host("https://api.github.com:8443/x"), Some("api.github.com"));
        assert_eq!(url_host("https://api.github.com?q=1"), Some("api.github.com"));
        assert_eq!(url_host("https://api.github.com#frag"), Some("api.github.com"));
    }

    #[test]
    fn userinfo_does_not_get_to_impersonate_a_host() {
        // The whole reason this is a function. A string comparison against
        // what follows `https://` approves both of these.
        assert_eq!(url_host("https://api.github.com@evil.example/"), Some("evil.example"));
        assert_eq!(url_host("https://user:pass@evil.example/"), Some("evil.example"));
        // An `@` inside the userinfo, so the split has to be from the right.
        assert_eq!(url_host("https://a@b@evil.example/"), Some("evil.example"));
    }

    #[test]
    fn anything_that_is_not_https_has_no_host() {
        assert_eq!(url_host("http://example.com"), None);
        assert_eq!(url_host("file:///etc/passwd"), None);
        assert_eq!(url_host("example.com"), None);
        assert_eq!(url_host("https://"), None);
        assert_eq!(url_host("https:///path"), None);
    }

    #[test]
    fn an_ipv6_literal_keeps_its_colons() {
        assert_eq!(url_host("https://[::1]/x"), Some("::1"));
        assert_eq!(url_host("https://[::1]:8443/x"), Some("::1"));
    }

    #[test]
    fn a_panel_tree_is_validated_on_the_way_in() {
        // The renderer never sees an unvalidated tree, so the cycle is caught
        // here rather than by whoever draws it.
        let tree = wit::ui::Tree {
            nodes: vec![wit::ui::Node { kind: wit::ui::NodeKind::Column, children: vec![0] }],
            root: 0,
        };
        assert_eq!(panel_from_wit(tree), Err(api::TreeError::NotATree(0)));
    }

    #[test]
    fn a_panel_tree_keeps_its_shape_and_its_text() {
        let tree = wit::ui::Tree {
            nodes: vec![
                wit::ui::Node { kind: wit::ui::NodeKind::Column, children: vec![1] },
                wit::ui::Node {
                    kind: wit::ui::NodeKind::Text(wit::ui::Text {
                        content: "hello".into(),
                        emphasis: wit::ui::Emphasis::Heading,
                    }),
                    children: vec![],
                },
            ],
            root: 0,
        };

        let panel = panel_from_wit(tree).unwrap();
        let root = panel.root().unwrap();
        assert_eq!(root.kind, api::NodeKind::Column);

        let child = panel.children(root).next().expect("the column has its text");
        assert_eq!(
            child.kind,
            api::NodeKind::Text { content: "hello".into(), emphasis: api::Emphasis::Heading },
        );
    }
}
