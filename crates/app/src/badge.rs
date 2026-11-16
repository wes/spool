//! The number on the Dock icon.
//!
//! One fact, in the one place someone looks when the window is not in front.
//! It is only worth having because the watcher keeps it true: a badge that is
//! accurate for a few seconds after each manual sync and stale for the hours
//! between is worse than no badge at all, because a badge showing nothing is a
//! claim that there is nothing new.
//!
//! Inbox folders only. Unread mail sitting in Junk or in an archive is not
//! something to interrupt anybody about, and a count that includes it is a
//! number nobody can act on.

use spool_core::{Mailbox, MailboxRole};

/// The number to show, or `None` when the icon should carry nothing.
///
/// Separated from the drawing so the rule can be tested without AppKit, which
/// is where the judgement is: *which* unread mail counts.
pub fn unread_label(mailboxes: &[Mailbox]) -> Option<String> {
    let unread: u32 = mailboxes
        .iter()
        .filter(|m| m.role == MailboxRole::Inbox)
        .map(|m| m.unread)
        .sum();

    match unread {
        0 => None,
        // Past four digits the badge is unreadable and the exact number has
        // stopped meaning anything different.
        n if n > 9_999 => Some("9999+".to_string()),
        n => Some(n.to_string()),
    }
}

/// Puts the count on the Dock icon.
#[cfg(target_os = "macos")]
pub fn set(mailboxes: &[Mailbox]) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    // Only from the main thread, and only when there is one to speak to.
    // Called from render, so this holds — but asking is free and being wrong
    // about it is an abort.
    let Some(mtm) = MainThreadMarker::new() else { return };

    let label = unread_label(mailboxes);
    let app = NSApplication::sharedApplication(mtm);
    let tile = app.dockTile();

    match label {
        Some(text) => tile.setBadgeLabel(Some(&NSString::from_str(&text))),
        // `None` clears it. An empty string would draw an empty badge, which
        // is a mark on the icon saying nothing.
        None => tile.setBadgeLabel(None),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set(_mailboxes: &[Mailbox]) {}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_core::{AccountId, MailboxId};

    fn folder(role: MailboxRole, unread: u32) -> Mailbox {
        Mailbox {
            id: MailboxId(1),
            account_id: AccountId(1),
            path: "x".into(),
            name: "x".into(),
            role,
            total: unread,
            unread,
            server_total: 0,
        }
    }

    #[test]
    fn the_badge_counts_inboxes_and_nothing_else() {
        // Unread mail in Junk or an archive is not something to interrupt
        // anybody about, and a count including it cannot be acted on.
        let folders = [
            folder(MailboxRole::Inbox, 3),
            folder(MailboxRole::Junk, 306),
            folder(MailboxRole::Archive, 2_000),
            folder(MailboxRole::Trash, 174),
        ];
        assert_eq!(unread_label(&folders), Some("3".into()));
    }

    #[test]
    fn every_account_adds_to_the_same_number() {
        let folders = [folder(MailboxRole::Inbox, 3), folder(MailboxRole::Inbox, 4)];
        assert_eq!(unread_label(&folders), Some("7".into()));
    }

    #[test]
    fn nothing_unread_is_no_badge_rather_than_a_zero() {
        // A badge reading `0` is a mark on the icon that says nothing, and
        // still draws the eye.
        assert_eq!(unread_label(&[folder(MailboxRole::Inbox, 0)]), None);
        assert_eq!(unread_label(&[]), None);
    }

    #[test]
    fn an_unreadable_number_is_capped() {
        assert_eq!(unread_label(&[folder(MailboxRole::Inbox, 12_000)]), Some("9999+".into()));
    }
}
