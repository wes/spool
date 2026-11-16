//! What the window is busy with, for the readout at the foot of the sidebar.
//!
//! The status bar already says what just happened, but it is off by default
//! and it holds one line that the next event overwrites. Neither of those is
//! what you want from work that is still running: a first sync of a large
//! account takes minutes, and for all of them the window otherwise looks
//! idle.
//!
//! So activities are tracked as a set rather than a single line. Syncing,
//! sending a message and creating a folder on the server can all be in flight
//! at once, and a single slot would mean whichever finished first switched the
//! indicator off while the others were still going.

/// The kinds of work worth telling someone about.
///
/// Only network work qualifies. Reads from the local store finish in
/// microseconds, and a progress bar that appears and vanishes between two
/// frames is a flicker, not information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Sync,
    /// Fetching mail older than anything stored, on request.
    ///
    /// Its own kind rather than a relabelled sync, and that is not a
    /// distinction without a difference: the two run on separate connections
    /// and can overlap, so sharing a kind would mean whichever finished first
    /// switched the indicator off while the other was still going — the exact
    /// failure this type is a set to prevent.
    Backfill,
    /// Creating, renaming or deleting a folder on the server.
    Folder,
    /// Replaying local changes — archives, moves, flags — onto the server.
    Push,
    Send,
}

/// One thing in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub kind: ActivityKind,
    /// What to show. Kept short: the sidebar is 214 points wide by default.
    pub label: String,
    /// How far along, between 0 and 1, when the work knows.
    ///
    /// `None` means genuinely unknown — waiting on a connection, waiting on a
    /// server to answer — and the bar sweeps rather than filling. Guessing a
    /// number for those would be a bar that stalls at 30% every time, which
    /// teaches people to distrust the ones that are real.
    pub fraction: Option<f32>,
}

/// Everything currently running.
#[derive(Debug, Default)]
pub struct Activities {
    /// In the order they started, so the newest is last.
    running: Vec<Activity>,
}

impl Activities {
    /// Starts an activity, or relabels the one of this kind already running.
    ///
    /// Starting the same kind twice is not an error: a second sync cannot be
    /// launched while the first runs, and for the ones that can overlap the
    /// honest reading is that the kind is still busy.
    pub fn begin(&mut self, kind: ActivityKind, label: impl Into<String>) {
        self.update(kind, label, None);
    }

    /// Sets the label and progress of an activity, starting it if it is not
    /// already running.
    pub fn update(
        &mut self,
        kind: ActivityKind,
        label: impl Into<String>,
        fraction: Option<f32>,
    ) {
        let label = label.into();
        let fraction = fraction.map(|f| f.clamp(0., 1.));

        match self.running.iter_mut().find(|a| a.kind == kind) {
            Some(activity) => {
                activity.label = label;
                // A determinate activity does not go back to sweeping because
                // one step could not say where it was. Progress that has been
                // established is kept until the activity ends.
                if fraction.is_some() {
                    activity.fraction = fraction;
                }
            }
            None => self.running.push(Activity { kind, label, fraction }),
        }
    }

    pub fn end(&mut self, kind: ActivityKind) {
        self.running.retain(|a| a.kind != kind);
    }

    /// How many are running. The readout shows one and counts the rest.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    /// The one to show.
    ///
    /// The most recently started, because that is the thing that just
    /// happened and the thing someone is waiting on. A long sync running
    /// underneath a folder rename should not hide the rename.
    pub fn current(&self) -> Option<&Activity> {
        self.running.last()
    }

    /// Everything running, newest first — the order the collapsed readout
    /// shows one of, so expanding continues the list rather than reordering
    /// it under the cursor.
    pub fn newest_first(&self) -> impl Iterator<Item = &Activity> {
        self.running.iter().rev()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_running_shows_nothing() {
        let activities = Activities::default();
        assert_eq!(activities.len(), 0);
        assert!(activities.current().is_none(), "and so the sidebar has no footer");
    }

    #[test]
    fn the_newest_activity_is_the_one_on_screen() {
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Sync, "Syncing\u{2026}");
        activities.begin(ActivityKind::Folder, "Creating Receipts\u{2026}");

        assert_eq!(activities.len(), 2);
        assert_eq!(activities.current().unwrap().kind, ActivityKind::Folder);
    }

    #[test]
    fn one_activity_ending_leaves_the_others_running() {
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Sync, "Syncing\u{2026}");
        activities.begin(ActivityKind::Send, "Sending\u{2026}");

        activities.end(ActivityKind::Send);

        assert_eq!(activities.len(), 1);
        assert_eq!(
            activities.current().unwrap().kind,
            ActivityKind::Sync,
            "the sync is still going and still has to say so"
        );
    }

    #[test]
    fn a_backfill_and_a_sync_do_not_switch_each_other_off() {
        // They run on separate connections and overlap freely. Sharing a kind
        // meant the first to finish ended the indicator for both, which is
        // what having a set of kinds is for.
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Sync, "Syncing\u{2026}");
        activities.begin(ActivityKind::Backfill, "Fetching older Inbox\u{2026}");

        activities.end(ActivityKind::Backfill);

        assert_eq!(activities.len(), 1);
        assert_eq!(activities.current().unwrap().kind, ActivityKind::Sync);
    }

    #[test]
    fn expanding_continues_the_collapsed_order_rather_than_reversing_it() {
        // The collapsed readout shows the newest. Expanding has to keep it at
        // the top, or the thing being watched jumps somewhere else the moment
        // the list opens.
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Sync, "Syncing\u{2026}");
        activities.begin(ActivityKind::Push, "Sending 12 changes\u{2026}");
        activities.begin(ActivityKind::Folder, "Creating Receipts\u{2026}");

        let order: Vec<_> = activities.newest_first().map(|a| a.kind).collect();
        assert_eq!(
            order,
            [ActivityKind::Folder, ActivityKind::Push, ActivityKind::Sync]
        );
        assert_eq!(activities.current().unwrap().kind, order[0]);
    }

    #[test]
    fn ending_something_that_never_started_is_not_an_error() {
        let mut activities = Activities::default();
        activities.end(ActivityKind::Sync);
        assert_eq!(activities.len(), 0);
    }

    #[test]
    fn beginning_a_kind_twice_relabels_it_rather_than_stacking() {
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Sync, "Connecting\u{2026}");
        activities.begin(ActivityKind::Sync, "Listing folders\u{2026}");

        assert_eq!(activities.len(), 1);
        assert_eq!(activities.current().unwrap().label, "Listing folders\u{2026}");
    }

    #[test]
    fn progress_established_is_not_given_up() {
        let mut activities = Activities::default();
        activities.update(ActivityKind::Sync, "Syncing INBOX", Some(0.4));
        // A step that cannot say where it is must not send the bar back to
        // sweeping: that reads as the work restarting.
        activities.update(ActivityKind::Sync, "INBOX: 12 new", None);

        assert_eq!(activities.current().unwrap().fraction, Some(0.4));
    }

    #[test]
    fn a_fraction_outside_the_bar_is_pulled_back_into_it() {
        let mut activities = Activities::default();
        activities.update(ActivityKind::Sync, "Syncing", Some(1.7));
        assert_eq!(activities.current().unwrap().fraction, Some(1.0));

        activities.update(ActivityKind::Sync, "Syncing", Some(-0.2));
        assert_eq!(activities.current().unwrap().fraction, Some(0.0));
    }

    #[test]
    fn an_activity_starts_out_indeterminate() {
        let mut activities = Activities::default();
        activities.begin(ActivityKind::Send, "Sending\u{2026}");
        assert_eq!(activities.current().unwrap().fraction, None);
    }
}
