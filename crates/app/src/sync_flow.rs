//! What a sync is doing, as three numbers.
//!
//! Folders are fetched over several connections at once, so "how far along is
//! this sync" stopped being a position in a list and became a population: at
//! any moment some folders are being fetched, some are still queued, and some
//! finished a moment ago. The bar at the foot of the sidebar draws that
//! population, and this is where its counts come from.
//!
//! Finished folders are counted over a five-second window rather than for the
//! whole run. A running total only ever grows, which makes a sync that has
//! stalled on one enormous folder look exactly like a sync that is flying;
//! a window drains when the work stops, so the bar goes visibly quiet and
//! being stuck looks like being stuck.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a finished folder goes on counting as just-finished.
pub const WINDOW: Duration = Duration::from_secs(5);

/// The smallest share of the bar a segment gets while it is not zero.
///
/// Without a floor the segment that matters most is the one you cannot see:
/// four connections working through six hundred folders is a loading segment
/// of two-thirds of one percent, which at this width is nothing at all. A
/// count of zero still draws nothing — the floor is for "few", not "none".
const FLOOR: f32 = 0.07;

/// The counts behind the bar, worked out for one instant.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Flow {
    /// Folders no connection has picked up yet.
    pub pending: usize,
    /// Folders being fetched right now — one per busy connection.
    pub loading: usize,
    /// Folders that finished inside the window.
    pub done: usize,
    /// How recently the last one finished: 1 as it lands, 0 once it has aged
    /// out. The glow at the head of the bar rides on this, which is what makes
    /// a completion something you see happen rather than a number that was
    /// different the last time you looked.
    pub freshness: f32,
}

impl Flow {
    /// Whether there is anything worth drawing.
    pub fn any(&self) -> bool {
        self.pending + self.loading + self.done > 0
    }

    /// The three segments as shares of the width, in bar order: done,
    /// loading, pending. They sum to 1 whenever anything is running.
    ///
    /// Not simply each count over the total, because the interesting segment
    /// is usually the smallest one. Every segment that exists is given `FLOOR`
    /// first and shares out what is left in proportion, so four busy
    /// connections stay visible against a queue of hundreds while the queue
    /// still plainly reads as the big one.
    pub fn shares(&self) -> (f32, f32, f32) {
        let counts = [self.done, self.loading, self.pending];
        let total: usize = counts.iter().sum();
        if total == 0 {
            return (0., 0., 0.);
        }

        let occupied = counts.iter().filter(|&&n| n > 0).count() as f32;
        // Enough segments that the floors alone fill the bar: nothing is left
        // to share out, so they simply split it. Only reachable if FLOOR is
        // ever raised past a third.
        let reserved = FLOOR * occupied;
        if reserved >= 1. {
            let each = 1. / occupied;
            let share = |n: usize| if n > 0 { each } else { 0. };
            return (share(counts[0]), share(counts[1]), share(counts[2]));
        }

        let spare = 1. - reserved;
        let share = |n: usize| match n {
            0 => 0.,
            n => FLOOR + spare * (n as f32 / total as f32),
        };
        (share(counts[0]), share(counts[1]), share(counts[2]))
    }
}

/// Everything one sync run has said about its folders.
#[derive(Debug, Default)]
pub struct SyncFlow {
    /// How many folders each account has to walk, by account index. Kept
    /// apart rather than summed because accounts sync at the same time and
    /// each one keeps re-announcing its own total.
    totals: Vec<usize>,
    /// When each recently finished folder finished, oldest first.
    completions: VecDeque<Instant>,
    /// Every folder finished this run, window or not. What `pending` is
    /// measured against, and the one number that must not forget.
    finished: usize,
}

impl SyncFlow {
    /// Clears everything for a new run.
    pub fn begin(&mut self) {
        self.totals.clear();
        self.completions.clear();
        self.finished = 0;
    }

    /// Records how many folders an account has, as the sync says so.
    ///
    /// The latest wins rather than the largest. Both things that report a
    /// count — the listing, and every folder opening — report the same
    /// already-narrowed number today, so this only matters if that ever stops
    /// being true: a queue drawn from the newest answer corrects itself on the
    /// next step, and one drawn from the biggest answer never comes down.
    pub fn note_total(&mut self, account: usize, total: usize) {
        if self.totals.len() <= account {
            self.totals.resize(account + 1, 0);
        }
        self.totals[account] = total;
    }

    /// Records a folder finishing.
    pub fn note_finished(&mut self, at: Instant) {
        self.finished += 1;
        self.completions.push_back(at);
        self.forget_old(at);
    }

    /// Drops completions that have aged out.
    ///
    /// Called on a timer as well as on arrival: the window has to keep
    /// draining after the last folder lands, or a sync that finishes leaves
    /// its final burst frozen on screen.
    pub fn forget_old(&mut self, now: Instant) {
        while let Some(&oldest) = self.completions.front() {
            if now.duration_since(oldest) < WINDOW {
                break;
            }
            self.completions.pop_front();
        }
    }

    /// The counts as of `now`.
    ///
    /// `loading` comes from the set of folders the sidebar is already keeping
    /// to put spinners on rows, rather than being counted again here: two
    /// tallies of the same thing drift, and the one with the rows attached is
    /// the one that has to be right.
    pub fn snapshot(&self, loading: usize, now: Instant) -> Flow {
        let total: usize = self.totals.iter().sum();
        // Saturating throughout: the totals arrive from several accounts over
        // the life of the run, and a folder can finish before the account it
        // belongs to has said how many it has.
        let pending = total.saturating_sub(self.finished).saturating_sub(loading);

        let done = self
            .completions
            .iter()
            .filter(|&&at| now.duration_since(at) < WINDOW)
            .count();

        let freshness = self
            .completions
            .back()
            .map(|&at| {
                let age = now.duration_since(at).as_secs_f32() / WINDOW.as_secs_f32();
                1. - age.clamp(0., 1.)
            })
            .unwrap_or(0.);

        Flow { pending, loading, done, freshness }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A time `seconds` into a test's own run.
    ///
    /// One origin per thread, taken once, so every `at` in a test is measured
    /// against the same instant. Reading the clock afresh each call made
    /// `at(0.)` a slightly later moment than the `at(0.)` a line above it,
    /// which is unobservable everywhere except an assertion about the moment a
    /// folder lands.
    fn at(seconds: f32) -> Instant {
        thread_local! {
            static ORIGIN: Instant = Instant::now();
        }
        ORIGIN.with(|origin| *origin) + Duration::from_secs_f32(seconds)
    }

    #[test]
    fn nothing_running_draws_nothing() {
        let flow = SyncFlow::default();
        assert!(!flow.snapshot(0, at(0.)).any());
    }

    #[test]
    fn the_queue_is_what_is_left_over() {
        let mut flow = SyncFlow::default();
        flow.note_total(0, 10);
        flow.note_finished(at(1.));
        flow.note_finished(at(1.));

        let snapshot = flow.snapshot(3, at(1.));
        assert_eq!(snapshot.done, 2);
        assert_eq!(snapshot.loading, 3);
        assert_eq!(snapshot.pending, 5, "ten folders, two done, three in flight");
    }

    #[test]
    fn several_accounts_add_up() {
        let mut flow = SyncFlow::default();
        flow.note_total(0, 10);
        flow.note_total(2, 4);
        assert_eq!(flow.snapshot(0, at(0.)).pending, 14);
    }

    #[test]
    fn a_revised_folder_count_replaces_the_one_before_it() {
        // The sync announces its folder count more than once. Should the two
        // ever disagree, the queue follows the newest rather than staying at
        // the high-water mark, which it could never come down from.
        let mut flow = SyncFlow::default();
        flow.note_total(0, 12);
        flow.note_total(0, 1);
        assert_eq!(flow.snapshot(0, at(0.)).pending, 1);
    }

    #[test]
    fn completions_leave_the_window_but_not_the_tally() {
        let mut flow = SyncFlow::default();
        flow.note_total(0, 10);
        flow.note_finished(at(0.));

        assert_eq!(flow.snapshot(0, at(1.)).done, 1);
        // Aged out of the five seconds...
        let later = flow.snapshot(0, at(6.));
        assert_eq!(later.done, 0, "the window has drained");
        // ...but the folder is still finished, so it is not back in the queue.
        assert_eq!(later.pending, 9, "and it did not un-finish");
    }

    #[test]
    fn a_stalled_sync_goes_quiet() {
        // The whole reason the count is a window. A cumulative total would
        // still read "40 done" while nothing at all was happening.
        let mut flow = SyncFlow::default();
        flow.note_total(0, 100);
        for _ in 0..40 {
            flow.note_finished(at(0.));
        }
        assert_eq!(flow.snapshot(1, at(0.5)).done, 40);
        assert_eq!(flow.snapshot(1, at(9.)).done, 0, "stuck on one big folder");
    }

    #[test]
    fn freshness_fades_over_the_window() {
        let mut flow = SyncFlow::default();
        flow.note_finished(at(0.));

        assert_eq!(flow.snapshot(0, at(0.)).freshness, 1.);
        let half = flow.snapshot(0, at(2.5)).freshness;
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert_eq!(flow.snapshot(0, at(5.)).freshness, 0.);
        assert_eq!(flow.snapshot(0, at(60.)).freshness, 0., "and stays there");
    }

    #[test]
    fn beginning_a_run_forgets_the_last_one() {
        let mut flow = SyncFlow::default();
        flow.note_total(0, 10);
        flow.note_finished(at(0.));

        flow.begin();
        assert!(!flow.snapshot(0, at(0.)).any());
    }

    #[test]
    fn the_segments_fill_the_bar() {
        let flow = Flow { done: 3, loading: 2, pending: 40, freshness: 0. };
        let (done, loading, pending) = flow.shares();
        assert!((done + loading + pending - 1.).abs() < 0.001);
    }

    #[test]
    fn a_few_busy_connections_stay_visible_against_a_huge_queue() {
        // Four connections against six hundred folders is two-thirds of one
        // percent, which at this width is not a segment. The floor is what
        // makes the thing you are actually watching visible.
        let flow = Flow { done: 0, loading: 4, pending: 600, freshness: 0. };
        let (_, loading, pending) = flow.shares();
        assert!(loading >= FLOOR, "{loading}");
        assert!(pending > loading * 5., "and the queue still reads as the big one");
    }

    #[test]
    fn a_segment_with_nothing_in_it_takes_no_room() {
        let flow = Flow { done: 0, loading: 2, pending: 8, freshness: 0. };
        let (done, loading, pending) = flow.shares();
        assert_eq!(done, 0., "the floor is for few, not for none");
        assert!((loading + pending - 1.).abs() < 0.001);
    }

    #[test]
    fn one_segment_alone_is_the_whole_bar() {
        let flow = Flow { done: 0, loading: 0, pending: 7, freshness: 0. };
        assert_eq!(flow.shares(), (0., 0., 1.));
    }

    #[test]
    fn the_bar_stays_whole_and_legible_the_whole_way_through_a_sync() {
        // The states a first sync actually passes through, in order. Each one
        // has to fill the bar exactly, and no segment that exists may be too
        // thin to see — the second of those is the case the floor is for, and
        // "four connections against forty-four folders" is where a purely
        // proportional bar loses the segment worth watching.
        let states = [
            ("just listed", Flow { done: 0, loading: 0, pending: 48, freshness: 0. }),
            ("first four open", Flow { done: 0, loading: 4, pending: 44, freshness: 0. }),
            ("warmed up", Flow { done: 6, loading: 4, pending: 30, freshness: 0.8 }),
            ("stalled on a big one", Flow { done: 0, loading: 1, pending: 6, freshness: 0. }),
            ("tail", Flow { done: 7, loading: 2, pending: 0, freshness: 0.9 }),
            ("last one lands", Flow { done: 3, loading: 0, pending: 0, freshness: 1. }),
        ];

        for (name, flow) in states {
            let (done, loading, pending) = flow.shares();
            assert!((done + loading + pending - 1.).abs() < 0.001, "{name} left a gap");

            for (share, count, part) in
                [(done, flow.done, "done"), (loading, flow.loading, "loading"),
                 (pending, flow.pending, "queued")]
            {
                match count {
                    0 => assert_eq!(share, 0., "{name}: an empty {part} took room"),
                    _ => assert!(share >= FLOOR, "{name}: {part} was too thin to see"),
                }
            }
        }
    }

    #[test]
    fn an_empty_flow_has_no_segments() {
        assert_eq!(Flow::default().shares(), (0., 0., 0.));
    }
}
