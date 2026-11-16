//! Instrumentation for the status bar.
//!
//! A development aid: query, open and frame timings in the status bar of a
//! debug build, so a regression is visible while working on the app. A
//! release build records none of it; the comparative numbers come from
//! `bench/`, measured from outside the app.

use std::time::Duration;

/// A short ring of recent frame times. Sixty is about half a second of
/// history: long enough to smooth out noise, short enough to react.
const WINDOW: usize = 60;

pub struct Perf {
    frames: [Duration; WINDOW],
    next_frame: usize,
    frames_seen: usize,

    /// Wall time of the last message-list query, including the hop to the
    /// background thread and back.
    pub last_query: Duration,
    /// How many rows that query returned.
    pub last_query_rows: usize,
    /// Wall time of the last body load and parse.
    pub last_open: Duration,
    /// Rows that fit on screen, used for page-wise movement.
    pub visible_rows: usize,
}

impl Default for Perf {
    fn default() -> Self {
        Self {
            frames: [Duration::ZERO; WINDOW],
            next_frame: 0,
            frames_seen: 0,
            last_query: Duration::ZERO,
            last_query_rows: 0,
            last_open: Duration::ZERO,
            visible_rows: 20,
        }
    }
}

impl Perf {
    pub fn record_frame(&mut self, elapsed: Duration) {
        self.frames[self.next_frame] = elapsed;
        self.next_frame = (self.next_frame + 1) % WINDOW;
        self.frames_seen = (self.frames_seen + 1).min(WINDOW);
    }

    // The readout these feed is shown only in a debug build, so a release
    // build keeps nothing.
    pub fn record_query(&mut self, elapsed: Duration, rows: usize) {
        if !cfg!(debug_assertions) {
            return;
        }
        self.last_query = elapsed;
        self.last_query_rows = rows;
    }

    pub fn record_open(&mut self, elapsed: Duration) {
        if !cfg!(debug_assertions) {
            return;
        }
        self.last_open = elapsed;
    }

    /// Mean time spent building the element tree, over the recent window.
    pub fn mean_frame(&self) -> Duration {
        if self.frames_seen == 0 {
            return Duration::ZERO;
        }
        let total: Duration = self.frames[..self.frames_seen].iter().sum();
        total / self.frames_seen as u32
    }

    /// The worst recent frame — the number that actually corresponds to a
    /// visible stutter, which an average will happily hide.
    pub fn worst_frame(&self) -> Duration {
        self.frames[..self.frames_seen].iter().copied().max().unwrap_or(Duration::ZERO)
    }
}

/// Formats a duration for the status bar, picking a unit that keeps three
/// significant digits.
pub fn format_duration(d: Duration) -> String {
    let micros = d.as_micros();
    if micros < 1_000 {
        format!("{micros}\u{00b5}s")
    } else if micros < 100_000 {
        format!("{:.1}ms", micros as f64 / 1_000.)
    } else {
        format!("{:.2}s", micros as f64 / 1_000_000.)
    }
}

/// Inserts thousands separators. Six-figure message counts are the whole
/// point, and `184029` does not read as a large number the way `184,029` does.
pub fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(184_029), "184,029");
        assert_eq!(format_count(1_000_000), "1,000,000");
    }

    #[test]
    fn durations_pick_a_readable_unit() {
        assert_eq!(format_duration(Duration::from_micros(250)), "250\u{00b5}s");
        assert_eq!(format_duration(Duration::from_micros(2_500)), "2.5ms");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1.50s");
    }

    #[test]
    fn frame_window_averages_and_wraps() {
        let mut perf = Perf::default();
        assert_eq!(perf.mean_frame(), Duration::ZERO);

        for _ in 0..WINDOW * 2 {
            perf.record_frame(Duration::from_millis(2));
        }
        assert_eq!(perf.mean_frame(), Duration::from_millis(2));

        perf.record_frame(Duration::from_millis(40));
        assert_eq!(perf.worst_frame(), Duration::from_millis(40));
    }
}
