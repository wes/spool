//! Where a panic goes.
//!
//! A panic in a desktop app is usually invisible: the window disappears and
//! the message went to a terminal nobody was watching — or, worse, nowhere at
//! all. This one is worse still on macOS, because most of our code runs inside
//! Cocoa callbacks: a panic there cannot unwind across the `extern "C"`
//! boundary, so the runtime calls `abort()` and the crash report says only
//! `abort() called`, with the panic's own message lost.
//!
//! So the message is written down before that happens. The hook runs *before*
//! the abort — that is the one moment the message still exists — and appends
//! it to a file next to the mail store, with a timestamp and a backtrace.

use std::{
    fs::OpenOptions,
    io::Write,
    panic::PanicHookInfo,
    path::{Path, PathBuf},
};

/// Keeps the log from growing without bound: past this it is rotated to
/// `panic.log.old`, so the last crash and the one before it are both kept and
/// nothing older is.
const MAX_LOG: u64 = 256 * 1024;

/// Where crashes are written.
pub fn log_path() -> PathBuf {
    spool_core::paths::data_dir().join("panic.log")
}

/// Installs the hook, keeping whatever the default hook does as well.
///
/// The default is kept deliberately: someone running from a terminal should
/// still see the panic where they are looking, and this adds a copy for the
/// times nobody is.
pub fn install() {
    install_to(log_path());
}

/// The same, against a path of the caller's choosing, so the hook itself can
/// be tested rather than only the pieces under it.
fn install_to(path: PathBuf) {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        // Written first. The default hook is what prints to stderr, and if
        // this panicked while printing we would rather have the file.
        write(&path, &describe(info));
        previous(info);
    }));
}

/// What a panic is worth writing down.
fn describe(info: &PanicHookInfo<'_>) -> String {
    let where_at = match info.location() {
        Some(location) => format!("{}:{}:{}", location.file(), location.line(), location.column()),
        None => "an unknown place".to_string(),
    };

    // `payload` is what `panic!` was handed: a `&str` for a literal, a
    // `String` for a formatted message, and something else entirely for a
    // panic from a library that threw a typed value.
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_string());

    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed").to_string();

    // Forced on, because the whole point of this file is being able to read it
    // after the fact: a report saying "run with RUST_BACKTRACE=1" is useless
    // when the thing to reproduce has already happened.
    let backtrace = std::backtrace::Backtrace::force_capture();

    format!(
        "\n--- {} \u{2014} spool {}\n\
         thread '{thread}' panicked at {where_at}:\n{message}\n\n{backtrace}\n",
        stamp(),
        env!("CARGO_PKG_VERSION"),
    )
}

/// Appends to the log, rotating it when it has grown past `MAX_LOG`.
///
/// Every failure here is swallowed: this runs while the process is already
/// dying, and a panic inside the panic hook aborts with even less to show for
/// it than the original.
fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
}

/// The time, as `2026-09-08 17:19:39Z`.
///
/// Hand-rolled from a Unix timestamp because the app has no date library and
/// one line of civil time is not worth adding one for. Days are counted from
/// the epoch with the proleptic Gregorian rules, which is exactly what a
/// timestamp means.
fn stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;

    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    let (year, month, day) = civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;

    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_and_a_known_date_both_come_out_right() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2026-09-08, the day this was written.
        assert_eq!(civil_from_days(20_704), (2026, 9, 8));
        // A leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn a_real_panic_goes_through_the_hook_and_into_the_file() {
        // The whole point, end to end: the hook runs while the panic is still
        // in flight, which on macOS is the only moment its message exists.
        let dir = std::env::temp_dir().join(format!("spool-hook-{}", std::process::id()));
        let path = dir.join("panic.log");
        std::fs::remove_dir_all(&dir).ok();

        let previous = std::panic::take_hook();
        install_to(path.clone());

        let caught = std::panic::catch_unwind(|| panic!("the roof fell in"));
        assert!(caught.is_err());

        std::panic::set_hook(previous);

        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(logged.contains("the roof fell in"), "the message: {logged}");
        assert!(logged.contains("crash.rs:"), "and where it happened: {logged}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_panic_is_written_where_it_can_be_read_afterwards() {
        let dir = std::env::temp_dir().join(format!("spool-panic-{}", std::process::id()));
        let path = dir.join("panic.log");

        write(&path, "first\n");
        write(&path, "second\n");
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(logged.contains("first") && logged.contains("second"), "appends, never replaces");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_log_that_has_grown_too_large_is_rotated_rather_than_trimmed() {
        let dir = std::env::temp_dir().join(format!("spool-panic-big-{}", std::process::id()));
        let path = dir.join("panic.log");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, vec![b'x'; MAX_LOG as usize + 1]).unwrap();
        write(&path, "after the rotation\n");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after the rotation\n");
        assert!(path.with_extension("log.old").exists(), "the old one is kept, not deleted");

        std::fs::remove_dir_all(&dir).ok();
    }
}
