//! Telling you about mail you were not watching arrive.
//!
//! Worth having only because the watcher exists. Before IDLE, every message
//! this could announce arrived during a sync you asked for and were already
//! looking at — a banner about the thing on your screen. Now mail lands while
//! the app is in the background, which is the moment a notification is the
//! only way you would know.
//!
//! Inbox only, and capped, both decided upstream in the sync: the folders mail
//! is *filed* into are not the folder it arrives in, and a first sync of two
//! thousand messages must not become two thousand banners.

use spool_net::Arrival;

/// What one banner says.
///
/// Split out from the delivery so the wording can be tested, which is where
/// the judgement is — the API call underneath has no decisions in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub title: String,
    pub body: String,
}

/// Turns what arrived into what to show.
///
/// One banner per message up to a handful, and a single counted one past
/// that: five notifications is a glance, twenty is a wall, and the number is
/// the news by then anyway.
pub fn banners(arrived: &[Arrival], total: usize) -> Vec<Banner> {
    const INDIVIDUALLY: usize = 3;

    if arrived.is_empty() {
        return Vec::new();
    }

    // More arrived than were described, or more than is worth describing.
    if total > INDIVIDUALLY {
        return vec![Banner {
            title: format!("{total} new messages"),
            body: summarize_senders(arrived),
        }];
    }

    arrived
        .iter()
        .map(|arrival| Banner {
            title: arrival.from.clone(),
            body: match arrival.subject.trim().is_empty() {
                true => "(no subject)".to_string(),
                false => arrival.subject.clone(),
            },
        })
        .collect()
}

/// "Ana Ruiz, Bo Chen and 3 others" — who it was, in one line.
fn summarize_senders(arrived: &[Arrival]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for arrival in arrived {
        let name = arrival.from.trim();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }

    match names.len() {
        0 => String::new(),
        1 => names[0].to_string(),
        2 => format!("{} and {}", names[0], names[1]),
        n => format!("{}, {} and {} others", names[0], names[1], n - 2),
    }
}

/// Posts the banners.
///
/// Silent about its own failures on purpose. A notification that could not be
/// delivered — permission withheld, an unbundled build, a system that has
/// notifications off — is not something to interrupt someone about a second
/// time, and the mail is in the list either way.
#[cfg(target_os = "macos")]
pub fn post(banners: &[Banner]) {
    use objc2_foundation::NSString;
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    if banners.is_empty() {
        return;
    }

    // Raises rather than returns an error in a process with no bundle
    // identifier, and an Objective-C exception here is an abort. The app is
    // bundled — see `scripts/bundle.sh` — but it is still run straight out of
    // `target/` during development, and that must not crash it.
    if !bundled() {
        log::debug!("not a bundled app; skipping {} notification(s)", banners.len());
        return;
    }

    let center = UNUserNotificationCenter::currentNotificationCenter();

    // Asked every time rather than once: the answer is the system's to keep,
    // this only has to not post before it has been given. A refusal makes the
    // posts below no-ops, which is the behaviour wanted.
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &block2::RcBlock::new(
            |_granted: objc2::runtime::Bool, _error: *mut objc2_foundation::NSError| {},
        ),
    );

    for banner in banners {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&banner.title));
        content.setBody(&NSString::from_str(&banner.body));

        // A fresh identifier each time: a repeated one replaces the banner
        // already on screen, which would mean two messages arriving showed one.
        let id = NSString::from_str(&format!(
            "mail-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));

        // No trigger: deliver now.
        let request =
            UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        center.addNotificationRequest_withCompletionHandler(&request, None);
    }
}

/// Whether this process has a bundle identifier, which is what the
/// notification centre insists on.
#[cfg(target_os = "macos")]
fn bundled() -> bool {
    use objc2_foundation::NSBundle;
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn post(_banners: &[Banner]) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(from: &str, subject: &str) -> Arrival {
        Arrival {
            account: "Work".into(),
            from: from.into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn one_message_is_named_by_who_sent_it() {
        let banners = banners(&[arrival("Ana Ruiz", "The proposal")], 1);
        assert_eq!(banners.len(), 1);
        assert_eq!(banners[0].title, "Ana Ruiz");
        assert_eq!(banners[0].body, "The proposal");
    }

    #[test]
    fn a_message_with_no_subject_says_so_rather_than_nothing() {
        // An empty body draws a banner with a blank second line, which reads
        // as a rendering fault rather than as a message without a subject.
        let banners = banners(&[arrival("Ana", "   ")], 1);
        assert_eq!(banners[0].body, "(no subject)");
    }

    #[test]
    fn a_handful_becomes_one_banner_with_the_count() {
        // Five banners is a wall. Past a few, the number is the news.
        let arrived = [
            arrival("Ana Ruiz", "One"),
            arrival("Bo Chen", "Two"),
            arrival("Cass", "Three"),
            arrival("Dara", "Four"),
        ];
        let banners = banners(&arrived, 4);

        assert_eq!(banners.len(), 1);
        assert_eq!(banners[0].title, "4 new messages");
        assert_eq!(banners[0].body, "Ana Ruiz, Bo Chen and 2 others");
    }

    #[test]
    fn the_count_is_of_everything_that_arrived_not_of_what_was_described() {
        // The sync only carries the first few; the banner still has to say how
        // many there really were.
        let banners = banners(&[arrival("Ana", "One"), arrival("Bo", "Two")], 2_000);
        assert_eq!(banners[0].title, "2000 new messages");
    }

    #[test]
    fn one_sender_who_sent_several_is_named_once() {
        let arrived = [
            arrival("Ana", "One"),
            arrival("Ana", "Two"),
            arrival("Ana", "Three"),
            arrival("Ana", "Four"),
        ];
        assert_eq!(banners(&arrived, 4)[0].body, "Ana");
    }

    #[test]
    fn nothing_arriving_says_nothing() {
        assert!(banners(&[], 0).is_empty());
    }
}
