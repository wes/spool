//! The viewer's clock.
//!
//! Timestamps are stored and compared as UTC seconds — the only sane thing to
//! do with mail that arrives from every zone there is. Display is the one
//! place that has to know where the reader is sitting, so the conversion lives
//! here and nowhere else, and `spool_core`'s formatters stay pure functions
//! that take the offset as an argument.

use std::cell::Cell;

/// Wall-clock seconds. Read once per visible range rather than per row.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Seconds east of UTC in the viewer's zone *at `ts`*.
///
/// At `ts` rather than now, because the offset is not a constant: a message
/// from July, read in January, was sent on July's clock, and rendering it with
/// today's offset moves it by an hour.
///
/// Memoised by day. `localtime_r` is not expensive, but the message list calls
/// this once per visible row per frame, and a mailbox is mostly runs of
/// messages from the same handful of days.
pub fn local_offset(ts: i64) -> i64 {
    thread_local! {
        /// The last (day, offset) answered. One entry is enough: rows are
        /// drawn in date order, so the hit rate is the run length.
        static CACHED: Cell<Option<(i64, i64)>> = const { Cell::new(None) };
    }

    let day = ts.div_euclid(86_400);
    CACHED.with(|cached| {
        if let Some((cached_day, offset)) = cached.get()
            && cached_day == day
        {
            return offset;
        }
        let offset = gmt_offset(ts);
        cached.set(Some((day, offset)));
        offset
    })
}

/// The zone offset the C library reports for `ts`, which is where the OS keeps
/// the answer: the user's chosen zone and its DST rules, kept current by the
/// system rather than by a table compiled into this binary.
fn gmt_offset(ts: i64) -> i64 {
    // SAFETY: `localtime_r` writes the `tm` it is handed and reads only the
    // `time_t`; both live for the call. Its result is checked for null before
    // the struct is read.
    unsafe {
        let time = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            // Nothing better to say than UTC — and better than no list at all.
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_is_a_whole_number_of_minutes_within_a_days_reach_of_utc() {
        // Every real zone is within UTC-12..=UTC+14 and lands on a minute.
        for ts in [0, 1_700_000_000, now_seconds()] {
            let offset = local_offset(ts);
            assert!((-12 * 3600..=14 * 3600).contains(&offset), "{offset} is not a zone");
            assert_eq!(offset % 60, 0, "{offset} is not a whole minute");
        }
    }

    #[test]
    fn the_memo_answers_what_the_library_does() {
        for ts in [0, 1_700_000_000, 1_709_210_040, now_seconds()] {
            assert_eq!(local_offset(ts), gmt_offset(ts));
        }
    }
}
